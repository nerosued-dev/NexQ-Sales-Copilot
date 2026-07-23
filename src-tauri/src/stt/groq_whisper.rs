// Sub-PRD 9: Groq Whisper REST API STT
//
// Groq provides an OpenAI-compatible transcription endpoint at:
//   https://api.groq.com/openai/v1/audio/transcriptions
//
// Gates audio locally into voice-confirmed configurable-length batches,
// then POSTs each batch as multipart form data.
// Uses Bearer token auth.
//
// Available models (March 2026):
//   - whisper-large-v3       (highest accuracy, 189x real-time, $0.111/hr)
//   - whisper-large-v3-turbo (fast + cheap, 216x real-time, $0.04/hr)
//
// Key behaviors:
//   - Local voice gate: only confirmed voice regions become queued batches
//   - Secondary silence defense: ready batches with RMS below threshold skip the API call
//   - Hallucination filter: known Whisper artifacts ("Thank you", etc.) are discarded
//   - Pause-based newlines: speech batches accumulate into one line until silence
//   - Live config updates: reads shared config Arc on each API call

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tauri::AppHandle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio::AudioChunk;
use crate::stt::groq_response::{
    decide_transcript_acceptance, normalize_response, parse_groq_response,
    NormalizedGroqTranscription, TranscriptAcceptance,
};
use crate::stt::local_voice_gate::{
    LocalVoiceGate, LocalVoiceGateConfig, ReadyBatch, VoiceBatchMetrics, VoiceGateState,
    FRAME_DURATION_MS,
};
use crate::stt::provider::{STTProvider, STTProviderType, TranscriptResult};

#[cfg(debug_assertions)]
fn transcript_diag_ts_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

macro_rules! transcript_diag {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        log::info!(
            "NEXQ_TRANSCRIPT_DIAG timestampMs={} component=groq {}",
            transcript_diag_ts_ms(),
            format_args!($($arg)*)
        );
    };
}

/// Sample rate expected by the audio pipeline (16 kHz mono).
const SAMPLE_RATE: u32 = 16000;
const GROQ_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Six default five-second mono PCM batches retain roughly 960 KiB per provider.
/// A bounded queue prevents an extended Groq outage from growing memory without limit.
/// Waiting for queue capacity deliberately applies backpressure to the audio pipeline;
/// callers must surface that stall rather than silently dropping a completed batch.
const GROQ_BATCH_QUEUE_CAPACITY: usize = 6;

/// RMS threshold for i16 PCM silence detection.
/// Segments with RMS below this are considered silence and skipped.
///
/// i16 range: -32768..32767. Typical values:
///   - Digital silence: 0-10
///   - Noise floor: 10-50
///   - Quiet speech: 100-500
///   - Normal speech: 500-3000
const SILENCE_RMS_THRESHOLD: f64 = 100.0;

// ── Groq Configuration ──

/// Configurable settings for the Groq Whisper STT provider.
/// Mirrors the Groq API parameters and is synced from the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroqConfig {
    /// Model ID: "whisper-large-v3" or "whisper-large-v3-turbo"
    pub model: String,
    /// ISO 639-1 language code (e.g. "en", "fr"). Empty = auto-detect.
    pub language: String,
    /// Sampling temperature (0.0–1.0). 0 = deterministic.
    pub temperature: f32,
    /// Response format: "json", "verbose_json", or "text"
    pub response_format: String,
    /// Timestamp granularities: "segment", "word", or both.
    pub timestamp_granularities: Vec<String>,
    /// Optional prompt to guide style/spelling (up to 224 tokens).
    pub prompt: String,
    /// Batch segment duration in seconds (how much audio to accumulate before sending).
    pub segment_duration_secs: f32,
}

impl Default for GroqConfig {
    fn default() -> Self {
        Self {
            model: "whisper-large-v3-turbo".to_string(),
            language: "en".to_string(),
            temperature: 0.0,
            response_format: "verbose_json".to_string(),
            timestamp_granularities: vec!["segment".to_string()],
            prompt: String::new(),
            segment_duration_secs: 5.0,
        }
    }
}

// ── Internal message types ──

/// Messages sent to the utterance accumulator task.
/// The accumulator tracks speech/silence boundaries to produce
/// pause-based newlines (like Deepgram's endpointing).
#[derive(Debug)]
enum AccumulatorMsg {
    /// A batch returned speech text — append to current utterance.
    Speech {
        text: String,
        confidence: Option<f32>,
        timestamp_ms: u64,
    },
    /// A batch was silent or a hallucination — may trigger utterance finalization.
    Silence,
    /// Stream ending — flush any accumulated text as final.
    Flush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroqRequestType {
    Normal,
    Residual,
}

impl GroqRequestType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Residual => "residual",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceBatchReason {
    MaxDuration,
    Endpoint,
    Finish,
}

impl VoiceBatchReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::MaxDuration => "max_duration",
            Self::Endpoint => "endpoint",
            Self::Finish => "finish",
        }
    }
}

struct GroqBatchJob {
    sequence: u64,
    samples: Vec<i16>,
    config: GroqConfig,
    request_type: GroqRequestType,
    ends_utterance: bool,
    gate_metrics: VoiceBatchMetrics,
    gate_reason: VoiceBatchReason,
    timestamp_ms: u64,
    audio_duration_secs: f32,
    rms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccumulatorBatchDecision {
    Speech,
    Silence,
}

struct TranscriptionRequestContext<'a> {
    party: &'a str,
    sequence: u64,
    request_type: &'a str,
    audio_duration_secs: f32,
    rms: f64,
    app_handle: Option<&'a AppHandle>,
}

#[async_trait]
trait GroqBatchProcessor: Send + Sync {
    async fn transcribe(
        &self,
        sequence: u64,
        samples: Vec<i16>,
        config: GroqConfig,
    ) -> Result<NormalizedGroqTranscription, String>;
}

struct HttpGroqBatchProcessor {
    api_key: String,
    app_handle: Option<AppHandle>,
}

#[async_trait]
impl GroqBatchProcessor for HttpGroqBatchProcessor {
    async fn transcribe(
        &self,
        _sequence: u64,
        samples: Vec<i16>,
        config: GroqConfig,
    ) -> Result<NormalizedGroqTranscription, String> {
        GroqWhisperSTT::call_api(&self.api_key, &config, samples, self.app_handle.as_ref()).await
    }
}

// ── GroqWhisperSTT provider ──

/// Groq Whisper REST API STT provider.
///
/// Uses a local per-stream voice gate to form configurable-length PCM batches,
/// then POSTs confirmed speech as WAV files to the Groq endpoint.
///
/// Uses an utterance accumulator task to produce pause-based newlines:
/// consecutive speech batches are joined into one line, and a silent batch
/// finalizes the current utterance (starts a new line).
pub struct GroqWhisperSTT {
    api_key: String,
    /// Local config (used as fallback when shared_config is unavailable).
    config: GroqConfig,
    /// Shared config Arc for live updates from settings UI.
    /// When present, the provider reads the latest config before each API call.
    shared_config: Option<Arc<RwLock<GroqConfig>>>,
    is_streaming: bool,
    result_tx: Option<mpsc::Sender<TranscriptResult>>,
    stop_flag: Arc<AtomicBool>,
    start_time: Option<Instant>,
    /// Per-stream acoustic gate. It is created on start and discarded on stop,
    /// so no candidate, pre-roll, or utterance state survives between captures.
    voice_gate: Option<LocalVoiceGate>,
    /// Segment counter for diagnostics.
    segment_counter: u64,
    /// Chunks fed since stream start (for periodic diagnostics).
    chunks_fed: u64,
    /// Tauri app handle for emitting debug events.
    app_handle: Option<AppHandle>,
    /// Party role ("You" or "Them") for event attribution.
    party: String,
    /// Channel to send results to the utterance accumulator task.
    accumulator_tx: Option<mpsc::Sender<AccumulatorMsg>>,
    accumulator_task: Option<JoinHandle<()>>,
    /// Bounded FIFO owned by this provider instance. A separate provider has a
    /// separate queue and worker, so You and Them can call Groq concurrently.
    batch_tx: Option<mpsc::Sender<GroqBatchJob>>,
    batch_worker_task: Option<JoinHandle<Result<(), String>>>,
    /// Diagnostic-only sequence assigned as each batch enters this provider's queue.
    /// It is never persisted or emitted to the frontend.
    next_batch_sequence: u64,
    batch_processor_override: Option<Arc<dyn GroqBatchProcessor>>,
}

impl GroqWhisperSTT {
    pub fn new() -> Self {
        Self {
            api_key: String::new(),
            config: GroqConfig::default(),
            shared_config: None,
            is_streaming: false,
            result_tx: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            start_time: None,
            voice_gate: None,
            segment_counter: 0,
            chunks_fed: 0,
            app_handle: None,
            party: String::new(),
            accumulator_tx: None,
            accumulator_task: None,
            batch_tx: None,
            batch_worker_task: None,
            next_batch_sequence: 0,
            batch_processor_override: None,
        }
    }

    /// Create with an API key.
    pub fn with_api_key(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            ..Self::new()
        }
    }

    /// Set the API key.
    pub fn set_api_key(&mut self, api_key: &str) {
        self.api_key = api_key.to_string();
    }

    /// Set the Tauri app handle for emitting debug events.
    pub fn set_app_handle(&mut self, handle: AppHandle) {
        self.app_handle = Some(handle);
    }

    /// Set the party role for event attribution.
    pub fn set_party(&mut self, party: &str) {
        self.party = party.to_string();
    }

    /// Set a shared config Arc for live updates from the settings UI.
    /// The provider reads the latest config before each API call, so
    /// settings changes take effect immediately on the next batch.
    pub fn set_shared_config(&mut self, config: Arc<RwLock<GroqConfig>>) {
        // Initialize local config from the shared one
        if let Ok(cfg) = config.read() {
            self.config = cfg.clone();
        }
        self.shared_config = Some(config);
    }

    /// Apply a GroqConfig directly (used in legacy single-provider path).
    pub fn set_config(&mut self, config: GroqConfig) {
        self.config = config;
    }

    /// Get the current config, preferring shared (live) over local.
    fn current_config(&self) -> GroqConfig {
        if let Some(ref arc) = self.shared_config {
            if let Ok(cfg) = arc.read() {
                return cfg.clone();
            }
        }
        self.config.clone()
    }

    fn voice_gate_config(config: &GroqConfig) -> Result<LocalVoiceGateConfig, String> {
        let requested_ms = f64::from(config.segment_duration_secs) * 1_000.0;
        if !requested_ms.is_finite() || requested_ms <= 0.0 {
            return Err("Groq segment duration must be finite and greater than zero".to_string());
        }

        let frame_duration_ms = f64::from(FRAME_DURATION_MS);
        let aligned_frames = (requested_ms / frame_duration_ms).round();
        let max_frames = f64::from(u32::MAX / FRAME_DURATION_MS);
        if aligned_frames < 1.0 || aligned_frames > max_frames {
            return Err("Groq segment duration is outside the local voice gate range".to_string());
        }

        let mut gate_config = LocalVoiceGateConfig::initial_test_hypothesis();
        gate_config.max_batch_ms = aligned_frames as u32 * FRAME_DURATION_MS;
        Ok(gate_config)
    }

    fn create_voice_gate(config: &GroqConfig) -> Result<LocalVoiceGate, String> {
        LocalVoiceGate::new(Self::voice_gate_config(config)?)
            .map_err(|error| format!("Local voice gate configuration failed: {error}"))
    }

    fn ready_batch_job(
        ready_batch: ReadyBatch,
        config: GroqConfig,
        request_type: GroqRequestType,
        gate_reason: VoiceBatchReason,
        timestamp_ms: u64,
    ) -> GroqBatchJob {
        let rms = Self::segment_rms(&ready_batch.samples);
        let audio_duration_secs = ready_batch.samples.len() as f32 / SAMPLE_RATE as f32;
        GroqBatchJob {
            sequence: 0,
            samples: ready_batch.samples,
            config,
            request_type,
            ends_utterance: ready_batch.ends_utterance,
            gate_metrics: ready_batch.metrics,
            gate_reason,
            timestamp_ms,
            audio_duration_secs,
            rms,
        }
    }

    async fn enqueue_batch(
        &mut self,
        mut job: GroqBatchJob,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let tx = self
            .batch_tx
            .as_ref()
            .cloned()
            .ok_or("Groq batch worker is not running")?;

        // Reserving a bounded slot is the explicit backpressure point. Sequence
        // assignment happens only after the slot is available, immediately before
        // the job enters this provider's FIFO.
        let permit = tx
            .reserve()
            .await
            .map_err(|_| "Groq batch worker stopped accepting jobs")?;
        let sequence = self
            .next_batch_sequence
            .checked_add(1)
            .ok_or("Groq batch sequence exhausted")?;
        self.next_batch_sequence = sequence;
        job.sequence = sequence;

        transcript_diag!(
            "event=batch_enqueued sequence={} party={} requestType={} gateReason={} endsUtterance={} audioDurationSecs={:.3} rms={:.1} totalFrames={} positiveFrames={} positiveRatio={:.3} longestPositiveRunFrames={} queueCapacity={}",
            job.sequence,
            self.party,
            job.request_type.as_str(),
            job.gate_reason.as_str(),
            job.ends_utterance,
            job.audio_duration_secs,
            job.rms,
            job.gate_metrics.total_frames,
            job.gate_metrics.positive_frames,
            job.gate_metrics.positive_ratio,
            job.gate_metrics.longest_positive_run_frames,
            GROQ_BATCH_QUEUE_CAPACITY
        );
        permit.send(job);
        Ok(())
    }

    /// Emit a debug event to both log and frontend DevLog.
    fn emit_debug(&self, level: &str, msg: &str) {
        match level {
            "error" => log::error!("GroqWhisperSTT[{}]: {}", self.party, msg),
            "warn" => log::warn!("GroqWhisperSTT[{}]: {}", self.party, msg),
            _ => log::info!("GroqWhisperSTT[{}]: {}", self.party, msg),
        }
        if let Some(ref handle) = self.app_handle {
            super::emit_stt_debug(handle, level, "groq", msg);
        }
    }

    /// Calculate RMS energy of i16 PCM samples.
    fn segment_rms(samples: &[i16]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt()
    }

    /// Encode accumulated PCM i16 samples as a WAV byte buffer (16-bit mono 16 kHz).
    fn encode_wav(samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let file_len = 36 + data_len;
        let mut buf = Vec::with_capacity(44 + data_len as usize);

        // RIFF header
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&file_len.to_le_bytes());
        buf.extend_from_slice(b"WAVE");

        // fmt sub-chunk
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&1u16.to_le_bytes()); // mono
        buf.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        buf.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
        buf.extend_from_slice(&2u16.to_le_bytes()); // block align
        buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        // data sub-chunk
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        for &sample in samples {
            buf.extend_from_slice(&sample.to_le_bytes());
        }

        buf
    }

    /// Make the API call and return the transcribed text (or None on error/silence).
    async fn call_api(
        api_key: &str,
        config: &GroqConfig,
        samples: Vec<i16>,
        app_handle: Option<&AppHandle>,
    ) -> Result<NormalizedGroqTranscription, String> {
        let duration_secs = samples.len() as f32 / SAMPLE_RATE as f32;
        let wav_data = Self::encode_wav(&samples);
        let wav_size_kb = wav_data.len() / 1024;

        if let Some(handle) = app_handle {
            super::emit_stt_debug(
                handle,
                "info",
                "groq",
                &format!(
                    "Sending {:.1}s audio ({} KB) to {} ...",
                    duration_secs, wav_size_kb, config.model
                ),
            );
        }

        let client = reqwest::Client::builder()
            .timeout(GROQ_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| "Groq HTTP client initialization failed".to_string())?;
        let file_part = reqwest::multipart::Part::bytes(wav_data)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));

        let mut form = reqwest::multipart::Form::new()
            .text("model", config.model.clone())
            .text("response_format", "verbose_json")
            .text("temperature", config.temperature.to_string())
            .part("file", file_part);

        // Add language if not empty and not "auto"
        let lang = &config.language;
        if !lang.is_empty() && lang != "auto" {
            let lang_code = lang.split('-').next().unwrap_or(lang);
            form = form.text("language", lang_code.to_string());
        }

        // Add prompt if provided
        if !config.prompt.is_empty() {
            form = form.text("prompt", config.prompt.clone());
        }

        // Segment granularity exposes Groq's quality metadata without the extra
        // latency of word-level timestamps.
        form = form.text("timestamp_granularities[]", "segment");

        let send_start = Instant::now();
        let response = client
            .post("https://api.groq.com/openai/v1/audio/transcriptions")
            .header("Authorization", format!("Bearer {}", api_key))
            .multipart(form)
            .send()
            .await;

        let latency_ms = send_start.elapsed().as_millis();

        match response {
            Ok(resp) => {
                if resp.status().is_success() {
                    let body = resp
                        .bytes()
                        .await
                        .map_err(|_| "Groq transcription response could not be read".to_string())?;
                    let parsed = parse_groq_response(&body).map_err(str::to_string)?;
                    let normalized = normalize_response(parsed);
                    if let Some(handle) = app_handle {
                        super::emit_stt_debug(
                            handle,
                            "info",
                            "groq",
                            &format!(
                                "Got verbose transcript metadata ({}ms, {} segments)",
                                latency_ms, normalized.segment_count
                            ),
                        );
                    }
                    Ok(normalized)
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    log::error!(
                        "GroqWhisperSTT: API returned status {} ({} response bytes)",
                        status,
                        body.len()
                    );
                    if let Some(handle) = app_handle {
                        super::emit_stt_debug(
                            handle,
                            "error",
                            "groq",
                            &format!("API error {} ({} response bytes)", status, body.len()),
                        );
                    }
                    Err(format!(
                        "Groq transcription request failed with status {}",
                        status
                    ))
                }
            }
            Err(e) => {
                log::error!("GroqWhisperSTT: Request failed: {}", e);
                if let Some(handle) = app_handle {
                    super::emit_stt_debug(
                        handle,
                        "error",
                        "groq",
                        &format!("Request failed: {}", e),
                    );
                }
                Err("Groq transcription request failed".to_string())
            }
        }
    }

    async fn forward_transcription(
        transcription: NormalizedGroqTranscription,
        accumulator_tx: &mpsc::Sender<AccumulatorMsg>,
        timestamp_ms: u64,
        context: TranscriptionRequestContext<'_>,
    ) -> AccumulatorBatchDecision {
        let acceptance = decide_transcript_acceptance(&transcription);
        let reason = acceptance
            .reason()
            .map(|value| value.as_str())
            .unwrap_or("none");
        transcript_diag!(
            "event=response_decision sequence={} party={} requestType={} audioDurationSecs={:.3} rms={:.1} segmentCount={} avgLogprobPresent={} avgLogprob={:?} noSpeechProbPresent={} noSpeechProb={:?} compressionRatioPresent={} compressionRatio={:?} timestampsPresent={} decision={} reason={}",
            context.sequence,
            context.party,
            context.request_type,
            context.audio_duration_secs,
            context.rms,
            transcription.segment_count,
            transcription.avg_logprob.is_some(),
            transcription.avg_logprob,
            transcription.no_speech_prob.is_some(),
            transcription.no_speech_prob,
            transcription.compression_ratio.is_some(),
            transcription.compression_ratio,
            transcription.has_timestamps,
            acceptance.decision_name(),
            reason
        );

        match acceptance {
            TranscriptAcceptance::Speech { confidence } => {
                let _ = accumulator_tx
                    .send(AccumulatorMsg::Speech {
                        text: transcription.text,
                        confidence,
                        timestamp_ms,
                    })
                    .await;
                AccumulatorBatchDecision::Speech
            }
            TranscriptAcceptance::Silence { .. } | TranscriptAcceptance::Rejected { .. } => {
                if let Some(handle) = context.app_handle {
                    super::emit_stt_debug(
                        handle,
                        "info",
                        "groq",
                        &format!("Transcription rejected ({reason})"),
                    );
                }
                let _ = accumulator_tx.send(AccumulatorMsg::Silence).await;
                AccumulatorBatchDecision::Silence
            }
        }
    }

    async fn run_batch_worker(
        mut batch_rx: mpsc::Receiver<GroqBatchJob>,
        processor: Arc<dyn GroqBatchProcessor>,
        accumulator_tx: mpsc::Sender<AccumulatorMsg>,
        party: String,
        app_handle: Option<AppHandle>,
    ) -> Result<(), String> {
        let mut first_error = None;

        while let Some(job) = batch_rx.recv().await {
            let GroqBatchJob {
                sequence,
                samples,
                config,
                request_type,
                ends_utterance,
                gate_metrics,
                gate_reason,
                timestamp_ms,
                audio_duration_secs,
                rms,
            } = job;
            let request_type_name = request_type.as_str();

            transcript_diag!(
                "event=request_started sequence={} party={} requestType={} gateReason={} endsUtterance={} audioDurationSecs={:.3} rms={:.1} totalFrames={} positiveFrames={} positiveRatio={:.3}",
                sequence,
                party,
                request_type_name,
                gate_reason.as_str(),
                ends_utterance,
                audio_duration_secs,
                rms,
                gate_metrics.total_frames,
                gate_metrics.positive_frames,
                gate_metrics.positive_ratio
            );

            // Every gate-approved batch still crosses the secondary RMS defense
            // inside the ordered worker. Its silence decision therefore cannot
            // overtake an earlier HTTP response.
            if rms < SILENCE_RMS_THRESHOLD {
                let _ = accumulator_tx.send(AccumulatorMsg::Silence).await;
                transcript_diag!(
                    "event=request_completed sequence={} party={} requestType={} gateReason={} endsUtterance={} outcome=local_silence",
                    sequence,
                    party,
                    request_type_name,
                    gate_reason.as_str(),
                    ends_utterance
                );
                continue;
            }

            let result = processor.transcribe(sequence, samples, config).await;
            transcript_diag!(
                "event=request_completed sequence={} party={} requestType={} outcome={}",
                sequence,
                party,
                request_type_name,
                if result.is_ok() { "result" } else { "failed" }
            );

            match result {
                Ok(transcription) => {
                    let decision = Self::forward_transcription(
                        transcription,
                        &accumulator_tx,
                        timestamp_ms,
                        TranscriptionRequestContext {
                            party: &party,
                            sequence,
                            request_type: request_type_name,
                            audio_duration_secs,
                            rms,
                            app_handle: app_handle.as_ref(),
                        },
                    )
                    .await;
                    if ends_utterance && decision == AccumulatorBatchDecision::Speech {
                        let _ = accumulator_tx.send(AccumulatorMsg::Silence).await;
                        transcript_diag!(
                            "event=utterance_boundary_applied sequence={} party={} requestType={} gateReason={} after=speech",
                            sequence,
                            party,
                            request_type_name,
                            gate_reason.as_str()
                        );
                    }
                }
                Err(error) => {
                    // Preserve the prior normal-request behavior: an API failure
                    // acts as a pause boundary. A final residual failure also
                    // closes the gate-confirmed utterance so it cannot remain open.
                    if request_type == GroqRequestType::Normal || ends_utterance {
                        let _ = accumulator_tx.send(AccumulatorMsg::Silence).await;
                    }
                    log::warn!(
                        "GroqWhisperSTT[{}]: batch {} ({}) failed",
                        party,
                        sequence,
                        request_type_name
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        transcript_diag!(
            "event=batch_worker_drained party={} outcome={}",
            party,
            if first_error.is_some() {
                "completed_with_error"
            } else {
                "completed"
            }
        );
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl STTProvider for GroqWhisperSTT {
    fn provider_name(&self) -> &str {
        "Groq Whisper"
    }

    fn provider_type(&self) -> STTProviderType {
        STTProviderType::GroqWhisper
    }

    async fn start_stream(
        &mut self,
        result_tx: mpsc::Sender<TranscriptResult>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.is_streaming {
            return Err("Stream already active".into());
        }

        if self.api_key.is_empty() {
            self.emit_debug("error", "API key not configured — cannot start");
            return Err("Groq API key not configured".into());
        }

        let config = self.current_config();
        let gate_config = Self::voice_gate_config(&config)?;
        let voice_gate = Self::create_voice_gate(&config)?;
        self.emit_debug(
            "info",
            &format!(
                "Starting stream: model={}, lang={}, segment={}s, temp={}",
                config.model, config.language, config.segment_duration_secs, config.temperature
            ),
        );

        // Set up the utterance accumulator — manages pause-based newlines.
        // Speech batches accumulate into one line; silence triggers finalization (new line).
        let (acc_tx, mut acc_rx) = mpsc::channel::<AccumulatorMsg>(64);
        self.accumulator_tx = Some(acc_tx.clone());
        self.result_tx = Some(result_tx.clone());

        let app_handle = self.app_handle.clone();
        let accumulator_party = self.party.clone();

        self.accumulator_task = Some(tokio::spawn(async move {
            let mut utt_counter: u64 = 0;
            let mut utt_text = String::new();
            let mut utt_id = String::new();
            let mut last_ts: u64 = 0;
            let mut utt_confidence: Option<f32> = None;

            while let Some(msg) = acc_rx.recv().await {
                match msg {
                    AccumulatorMsg::Speech {
                        text,
                        confidence,
                        timestamp_ms,
                    } => {
                        // Start a new utterance if needed
                        if utt_text.is_empty() {
                            utt_counter += 1;
                            utt_id = format!("groq_utt_{}", utt_counter);
                        }
                        transcript_diag!(
                            "event=accumulator_received kind=speech party={} segmentId={} utteranceCount={}",
                            accumulator_party,
                            utt_id,
                            utt_counter
                        );

                        // Append this batch's text to the current utterance
                        if !utt_text.is_empty() {
                            utt_text.push(' ');
                        }
                        utt_text.push_str(&text);
                        last_ts = timestamp_ms;
                        utt_confidence = match (utt_confidence, confidence) {
                            (Some(current), Some(next)) => Some(current.min(next)),
                            (None, value) | (value, None) => value,
                        };

                        // Emit as interim — frontend updates in-place (same line)
                        let _ = result_tx
                            .send(TranscriptResult {
                                text: utt_text.clone(),
                                is_final: false,
                                confidence: utt_confidence.unwrap_or(0.0),
                                timestamp_ms,
                                speaker: None,
                                language: None,
                                segment_id: Some(utt_id.clone()),
                            })
                            .await;
                        transcript_diag!(
                            "event=accumulator_result_sent kind=partial party={} segmentId={} utteranceCount={}",
                            accumulator_party,
                            utt_id,
                            utt_counter
                        );
                    }
                    message => {
                        let message_kind = match message {
                            AccumulatorMsg::Silence => "silence",
                            AccumulatorMsg::Flush => "flush",
                            AccumulatorMsg::Speech { .. } => "speech",
                        };
                        transcript_diag!(
                            "event=accumulator_received kind={} party={} segmentId={} utteranceCount={}",
                            message_kind,
                            accumulator_party,
                            utt_id,
                            utt_counter
                        );
                        if !utt_text.is_empty() {
                            // Finalize the current utterance — frontend starts new line
                            let _ = result_tx
                                .send(TranscriptResult {
                                    text: utt_text.clone(),
                                    is_final: true,
                                    confidence: utt_confidence.unwrap_or(0.0),
                                    timestamp_ms: last_ts,
                                    speaker: None,
                                    language: None,
                                    segment_id: Some(utt_id.clone()),
                                })
                                .await;
                            transcript_diag!(
                                "event=accumulator_result_sent kind=final party={} segmentId={} trigger={} utteranceCount={}",
                                accumulator_party,
                                utt_id,
                                message_kind,
                                utt_counter
                            );
                            utt_text.clear();
                            utt_confidence = None;

                            if let Some(ref handle) = app_handle {
                                super::emit_stt_debug(
                                    handle,
                                    "info",
                                    "groq",
                                    "Utterance finalized (pause detected)",
                                );
                            }
                        }
                    }
                }
            }
            transcript_diag!(
                "event=accumulator_channel_closed party={} segmentId={} utteranceCount={}",
                accumulator_party,
                utt_id,
                utt_counter
            );

            // Flush remaining text when channel closes (stream ending)
            if !utt_text.is_empty() {
                let _ = result_tx
                    .send(TranscriptResult {
                        text: utt_text,
                        is_final: true,
                        confidence: utt_confidence.unwrap_or(0.0),
                        timestamp_ms: last_ts,
                        speaker: None,
                        language: None,
                        segment_id: Some(utt_id.clone()),
                    })
                    .await;
                transcript_diag!(
                    "event=accumulator_result_sent kind=final party={} segmentId={} trigger=channel_closed utteranceCount={}",
                    accumulator_party,
                    utt_id,
                    utt_counter
                );
            }
        }));

        let processor: Arc<dyn GroqBatchProcessor> =
            self.batch_processor_override.clone().unwrap_or_else(|| {
                Arc::new(HttpGroqBatchProcessor {
                    api_key: self.api_key.clone(),
                    app_handle: self.app_handle.clone(),
                })
            });
        let (batch_tx, batch_rx) = mpsc::channel(GROQ_BATCH_QUEUE_CAPACITY);
        self.batch_tx = Some(batch_tx);
        self.batch_worker_task = Some(tokio::spawn(Self::run_batch_worker(
            batch_rx,
            processor,
            acc_tx,
            self.party.clone(),
            self.app_handle.clone(),
        )));

        self.is_streaming = true;
        self.stop_flag.store(false, Ordering::SeqCst);
        self.start_time = Some(Instant::now());
        self.voice_gate = Some(voice_gate);
        self.segment_counter = 0;
        self.chunks_fed = 0;
        self.next_batch_sequence = 0;

        transcript_diag!(
            "event=voice_gate_created party={} state=idle maxBatchMs={} hypothesis=initial_test",
            self.party,
            gate_config.max_batch_ms
        );
        self.emit_debug(
            "info",
            "Stream started: local voice gate is selecting Groq batches",
        );
        Ok(())
    }

    async fn feed_audio(
        &mut self,
        chunk: AudioChunk,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_streaming || self.stop_flag.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.chunks_fed += 1;

        // Log first chunk and periodic stats
        if self.chunks_fed == 1 {
            self.emit_debug("info", "First audio chunk received");
        }
        if self.chunks_fed % 500 == 0 {
            if let Some(gate) = self.voice_gate.as_ref() {
                self.emit_debug(
                    "info",
                    &format!(
                        "Voice gate: state={:?}, pendingSamples={}, chunksFed={}",
                        gate.state(),
                        gate.remainder_len(),
                        self.chunks_fed
                    ),
                );
            }
        }

        let received_samples = chunk.pcm_data.len();
        let (previous_state, new_state, pending_samples, ready_batches) = {
            let gate = self
                .voice_gate
                .as_mut()
                .ok_or("Local voice gate is not initialized")?;
            let previous_state = gate.state();
            let ready_batches = gate.push_samples(&chunk.pcm_data);
            (
                previous_state,
                gate.state(),
                gate.remainder_len(),
                ready_batches,
            )
        };

        if previous_state != new_state {
            let reason = match (previous_state, new_state) {
                (VoiceGateState::Idle, VoiceGateState::Candidate) => "candidate_started",
                (VoiceGateState::Candidate, VoiceGateState::Active) => "activation",
                (VoiceGateState::Active, VoiceGateState::Hangover) => "hangover",
                (VoiceGateState::Hangover, VoiceGateState::Active) => "resumption",
                (_, VoiceGateState::Idle) => "endpoint_or_rejection",
                _ => "state_transition",
            };
            transcript_diag!(
                "event=voice_gate_transition party={} previousState={:?} newState={:?} receivedSamples={} pendingSamples={} reason={}",
                self.party,
                previous_state,
                new_state,
                received_samples,
                pending_samples,
                reason
            );
        }

        let timestamp_ms = self
            .start_time
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(chunk.timestamp_ms);
        for ready_batch in ready_batches {
            self.segment_counter += 1;
            let gate_reason = if ready_batch.ends_utterance {
                VoiceBatchReason::Endpoint
            } else {
                VoiceBatchReason::MaxDuration
            };
            let job = Self::ready_batch_job(
                ready_batch,
                self.current_config(),
                GroqRequestType::Normal,
                gate_reason,
                timestamp_ms,
            );
            self.emit_debug(
                "info",
                &format!(
                    "Voice batch #{}: reason={}, duration={:.3}s, endsUtterance={}, rms={:.1}",
                    self.segment_counter,
                    gate_reason.as_str(),
                    job.audio_duration_secs,
                    job.ends_utterance,
                    job.rms
                ),
            );
            self.enqueue_batch(job).await?;
        }

        Ok(())
    }

    async fn stop_stream(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_streaming {
            return Ok(());
        }

        self.emit_debug(
            "info",
            &format!(
                "Stopping stream — {} chunks fed, {} segments sent",
                self.chunks_fed, self.segment_counter
            ),
        );

        self.stop_flag.store(true, Ordering::SeqCst);

        let mut first_error = None;

        // Finalize the same gate that accepted every stream sample. Confirmed
        // speech residuals are queued regardless of duration; candidates that
        // never activated are discarded by LocalVoiceGate::finish.
        let final_batches = match self.voice_gate.take() {
            Some(mut gate) => {
                let previous_state = gate.state();
                let batches = gate.finish();
                transcript_diag!(
                    "event=voice_gate_finished party={} previousState={:?} newState={:?} batchesProduced={}",
                    self.party,
                    previous_state,
                    gate.state(),
                    batches.len()
                );
                batches
            }
            None => {
                first_error = Some("Local voice gate was unavailable during shutdown".to_string());
                Vec::new()
            }
        };
        let timestamp_ms = self
            .start_time
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);
        for ready_batch in final_batches {
            self.segment_counter += 1;
            let gate_reason = if ready_batch.ends_utterance {
                VoiceBatchReason::Finish
            } else {
                VoiceBatchReason::MaxDuration
            };
            let job = Self::ready_batch_job(
                ready_batch,
                self.current_config(),
                GroqRequestType::Residual,
                gate_reason,
                timestamp_ms,
            );
            self.emit_debug(
                "info",
                &format!(
                    "Final voice batch #{}: reason={}, duration={:.3}s, endsUtterance={}, rms={:.1}",
                    self.segment_counter,
                    gate_reason.as_str(),
                    job.audio_duration_secs,
                    job.ends_utterance,
                    job.rms
                ),
            );
            if let Err(error) = self.enqueue_batch(job).await {
                if first_error.is_none() {
                    first_error = Some(error.to_string());
                }
            }
        }

        // Closing the only producer lets the per-provider worker drain every
        // normal batch followed by the optional residual. No worker is detached.
        self.batch_tx = None;
        if let Some(task) = self.batch_worker_task.take() {
            let worker_error = match task.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("Groq batch worker terminated unexpectedly".to_string()),
            };
            if first_error.is_none() {
                first_error = worker_error;
            }
        } else if first_error.is_none() {
            first_error = Some("Groq batch worker was unavailable during shutdown".to_string());
        }

        // Flush the accumulator (finalize any accumulated utterance)
        if let Some(ref tx) = self.accumulator_tx {
            transcript_diag!(
                "event=accumulator_flush_sent party={} batchCount={}",
                self.party,
                self.next_batch_sequence
            );
            let _ = tx.send(AccumulatorMsg::Flush).await;
        }

        // Drop channels to let the accumulator task finish
        self.accumulator_tx = None;
        if let Some(task) = self.accumulator_task.take() {
            if task.await.is_err() && first_error.is_none() {
                first_error =
                    Some("Groq transcript accumulator terminated unexpectedly".to_string());
            }
        }
        self.is_streaming = false;
        self.result_tx = None;
        self.start_time = None;

        self.emit_debug("info", "Stream stopped");
        match first_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    async fn test_connection(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if self.api_key.is_empty() {
            return Err("No API key configured".into());
        }

        log::info!("GroqWhisperSTT: Testing connection...");

        // Test by listing models — a lightweight authenticated endpoint
        let client = reqwest::Client::new();
        let response = client
            .get("https://api.groq.com/openai/v1/models")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await?;

        let success = response.status().is_success();
        if success {
            log::info!("GroqWhisperSTT: Connection test passed");
        } else {
            log::warn!(
                "GroqWhisperSTT: Connection test failed with status {}",
                response.status()
            );
        }

        Ok(success)
    }

    fn set_language(&mut self, language: &str) {
        self.config.language = language.to_string();
        log::info!("GroqWhisperSTT: Language set to {}", self.config.language);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::{oneshot, Mutex as AsyncMutex};

    type MockOutcome = Result<NormalizedGroqTranscription, String>;

    struct ControlledProcessor {
        started_tx: mpsc::Sender<u64>,
        releases: AsyncMutex<HashMap<u64, oneshot::Receiver<MockOutcome>>>,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ControlledProcessor {
        fn observe_active(&self, current: usize) {
            let mut observed = self.max_active.load(Ordering::SeqCst);
            while current > observed {
                match self.max_active.compare_exchange(
                    observed,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
        }
    }

    #[async_trait]
    impl GroqBatchProcessor for ControlledProcessor {
        async fn transcribe(
            &self,
            sequence: u64,
            _samples: Vec<i16>,
            _config: GroqConfig,
        ) -> MockOutcome {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.observe_active(active);
            if self.started_tx.send(sequence).await.is_err() {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err("mock start observer closed".to_string());
            }
            let release = self.releases.lock().await.remove(&sequence);
            let outcome = match release {
                Some(release) => release
                    .await
                    .unwrap_or_else(|_| Err("mock release dropped".to_string())),
                None => Err("mock release missing".to_string()),
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            outcome
        }
    }

    struct ScriptedProcessor {
        outcomes: AsyncMutex<HashMap<u64, MockOutcome>>,
        calls: AsyncMutex<Vec<u64>>,
    }

    #[async_trait]
    impl GroqBatchProcessor for ScriptedProcessor {
        async fn transcribe(
            &self,
            sequence: u64,
            _samples: Vec<i16>,
            _config: GroqConfig,
        ) -> MockOutcome {
            self.calls.lock().await.push(sequence);
            self.outcomes
                .lock()
                .await
                .remove(&sequence)
                .unwrap_or_else(|| Err("mock outcome missing".to_string()))
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        sequence: u64,
        samples: Vec<i16>,
    }

    struct RecordingProcessor {
        calls: AsyncMutex<Vec<RecordedRequest>>,
    }

    #[async_trait]
    impl GroqBatchProcessor for RecordingProcessor {
        async fn transcribe(
            &self,
            sequence: u64,
            samples: Vec<i16>,
            _config: GroqConfig,
        ) -> MockOutcome {
            self.calls
                .lock()
                .await
                .push(RecordedRequest { sequence, samples });
            Ok(transcription("synthetic speech"))
        }
    }

    fn transcription(text: &str) -> NormalizedGroqTranscription {
        NormalizedGroqTranscription {
            text: text.to_string(),
            segment_count: 1,
            avg_logprob: Some(-0.1),
            no_speech_prob: Some(0.01),
            compression_ratio: Some(1.0),
            has_timestamps: true,
        }
    }

    fn gate_metrics(real_samples: usize, positive_frames: usize) -> VoiceBatchMetrics {
        let total_frames = real_samples.div_ceil(crate::stt::local_voice_gate::FRAME_SAMPLES);
        VoiceBatchMetrics {
            total_frames,
            positive_frames,
            positive_ratio: if total_frames == 0 {
                0.0
            } else {
                positive_frames as f32 / total_frames as f32
            },
            positive_samples: real_samples,
            positive_duration_ms: real_samples as f64 * 1_000.0 / SAMPLE_RATE as f64,
            longest_positive_run_frames: positive_frames,
            longest_positive_run_ms: positive_frames as f64 * f64::from(FRAME_DURATION_MS),
            longest_silence_run_frames: 0,
            longest_silence_run_ms: 0.0,
            real_samples,
            total_duration_ms: real_samples as f64 * 1_000.0 / SAMPLE_RATE as f64,
        }
    }

    fn batch_job(sequence: u64, request_type: GroqRequestType) -> GroqBatchJob {
        let samples = vec![200; 16];
        GroqBatchJob {
            sequence,
            gate_metrics: gate_metrics(samples.len(), 1),
            samples,
            config: GroqConfig::default(),
            request_type,
            ends_utterance: false,
            gate_reason: VoiceBatchReason::MaxDuration,
            timestamp_ms: sequence * 100,
            audio_duration_secs: 0.001,
            rms: 200.0,
        }
    }

    fn silent_batch_job(sequence: u64) -> GroqBatchJob {
        GroqBatchJob {
            samples: Vec::new(),
            gate_metrics: gate_metrics(0, 0),
            rms: 0.0,
            ..batch_job(sequence, GroqRequestType::Normal)
        }
    }

    fn audio_chunk(samples: usize, timestamp_ms: u64) -> AudioChunk {
        pcm_chunk(vec![1_000; samples], timestamp_ms, false)
    }

    fn pcm_chunk(pcm_data: Vec<i16>, timestamp_ms: u64, is_speech: bool) -> AudioChunk {
        AudioChunk {
            pcm_data,
            source: crate::audio::AudioSource::Mic,
            timestamp_ms,
            is_speech,
        }
    }

    fn pcm_frames(value: i16, frames: usize) -> Vec<i16> {
        vec![value; frames * crate::stt::local_voice_gate::FRAME_SAMPLES]
    }

    fn controlled_processor(
        sequences: &[u64],
    ) -> (
        Arc<ControlledProcessor>,
        mpsc::Receiver<u64>,
        HashMap<u64, oneshot::Sender<MockOutcome>>,
    ) {
        let (started_tx, started_rx) = mpsc::channel(sequences.len().max(1));
        let mut release_receivers = HashMap::new();
        let mut release_senders = HashMap::new();
        for &sequence in sequences {
            let (release_tx, release_rx) = oneshot::channel();
            release_senders.insert(sequence, release_tx);
            release_receivers.insert(sequence, release_rx);
        }
        (
            Arc::new(ControlledProcessor {
                started_tx,
                releases: AsyncMutex::new(release_receivers),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }),
            started_rx,
            release_senders,
        )
    }

    fn scripted_processor(
        outcomes: impl IntoIterator<Item = (u64, MockOutcome)>,
    ) -> Arc<ScriptedProcessor> {
        Arc::new(ScriptedProcessor {
            outcomes: AsyncMutex::new(outcomes.into_iter().collect()),
            calls: AsyncMutex::new(Vec::new()),
        })
    }

    fn recording_processor() -> Arc<RecordingProcessor> {
        Arc::new(RecordingProcessor {
            calls: AsyncMutex::new(Vec::new()),
        })
    }

    async fn recording_provider(
        segment_duration_secs: f32,
    ) -> (
        GroqWhisperSTT,
        Arc<RecordingProcessor>,
        mpsc::Receiver<TranscriptResult>,
    ) {
        let processor = recording_processor();
        let mut provider = GroqWhisperSTT::with_api_key("test-key");
        provider.set_party("You");
        provider.set_config(GroqConfig {
            segment_duration_secs,
            ..GroqConfig::default()
        });
        provider.batch_processor_override = Some(processor.clone());
        let (result_tx, result_rx) = mpsc::channel(16);
        provider.start_stream(result_tx).await.unwrap();
        (provider, processor, result_rx)
    }

    async fn assert_signal_makes_no_request(pcm_data: Vec<i16>, is_speech: bool) {
        let (mut provider, processor, mut result_rx) = recording_provider(5.0).await;
        provider
            .feed_audio(pcm_chunk(pcm_data, 100, is_speech))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();
        assert!(processor.calls.lock().await.is_empty());
        assert_eq!(provider.next_batch_sequence, 0);
        assert!(result_rx.recv().await.is_none());
    }

    fn spawn_test_worker(
        processor: Arc<dyn GroqBatchProcessor>,
        party: &str,
        capacity: usize,
    ) -> (
        mpsc::Sender<GroqBatchJob>,
        mpsc::Receiver<AccumulatorMsg>,
        JoinHandle<Result<(), String>>,
    ) {
        let (batch_tx, batch_rx) = mpsc::channel(capacity);
        let (accumulator_tx, accumulator_rx) = mpsc::channel(32);
        let worker = tokio::spawn(GroqWhisperSTT::run_batch_worker(
            batch_rx,
            processor,
            accumulator_tx,
            party.to_string(),
            None,
        ));
        (batch_tx, accumulator_rx, worker)
    }

    async fn receive_speech(receiver: &mut mpsc::Receiver<AccumulatorMsg>) -> String {
        match receiver.recv().await {
            Some(AccumulatorMsg::Speech { text, .. }) => text,
            other => panic!("expected speech, got {other:?}"),
        }
    }

    async fn expect_silence(receiver: &mut mpsc::Receiver<AccumulatorMsg>) {
        assert!(matches!(
            receiver.recv().await,
            Some(AccumulatorMsg::Silence)
        ));
    }

    #[test]
    fn configured_segment_duration_sets_voice_gate_maximum() {
        let config = GroqConfig {
            segment_duration_secs: 2.5,
            ..GroqConfig::default()
        };
        assert_eq!(
            GroqWhisperSTT::voice_gate_config(&config)
                .unwrap()
                .max_batch_ms,
            2_500
        );
    }

    #[tokio::test]
    async fn local_gate_blocks_silence_noise_impulses_and_unconfirmed_candidates() {
        assert_signal_makes_no_request(pcm_frames(0, 150), true).await;
        assert_signal_makes_no_request(pcm_frames(299, 100), true).await;

        let mut isolated_impulse = pcm_frames(1_000, 1);
        isolated_impulse.extend(pcm_frames(0, 9));
        assert_signal_makes_no_request(isolated_impulse, false).await;

        let mut separated_impulses = pcm_frames(1_000, 1);
        separated_impulses.extend(pcm_frames(0, 1));
        separated_impulses.extend(pcm_frames(1_000, 1));
        separated_impulses.extend(pcm_frames(0, 7));
        assert_signal_makes_no_request(separated_impulses, false).await;

        assert_signal_makes_no_request(pcm_frames(1_000, 1), false).await;
    }

    #[tokio::test]
    async fn confirmed_short_speech_is_sent_even_below_half_a_second() {
        let (mut provider, processor, mut result_rx) = recording_provider(5.0).await;
        let short_speech = pcm_frames(1_000, 3);
        provider
            .feed_audio(pcm_chunk(short_speech.clone(), 100, false))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();

        let calls = processor.calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].sequence, 1);
        assert_eq!(calls[0].samples, short_speech);
        assert_eq!(calls[0].samples.len(), 960);
        assert!(calls[0].samples.len() < SAMPLE_RATE as usize / 2);

        let partial = result_rx.recv().await.unwrap();
        let final_result = result_rx.recv().await.unwrap();
        assert!(!partial.is_final);
        assert!(final_result.is_final);
        assert_eq!(partial.text, final_result.text);
        assert!(result_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn gate_to_job_preserves_pre_roll_internal_silence_and_every_sample() {
        let (mut provider, processor, _result_rx) = recording_provider(5.0).await;
        let pre_roll = pcm_frames(0, 15);
        let confirmed = pcm_frames(1_000, 3);
        let internal_silence = pcm_frames(0, 2);
        let resumed = pcm_frames(1_000, 1);

        provider
            .feed_audio(pcm_chunk(pre_roll.clone(), 100, true))
            .await
            .unwrap();
        provider
            .feed_audio(pcm_chunk(confirmed.clone(), 120, false))
            .await
            .unwrap();
        provider
            .feed_audio(pcm_chunk(internal_silence.clone(), 140, true))
            .await
            .unwrap();
        provider
            .feed_audio(pcm_chunk(resumed.clone(), 160, false))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();

        let mut expected = pre_roll;
        expected.extend(confirmed);
        expected.extend(internal_silence.clone());
        expected.extend(resumed);
        let calls = processor.calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].samples, expected);
        let internal_start = 18 * crate::stt::local_voice_gate::FRAME_SAMPLES;
        let internal_end = internal_start + internal_silence.len();
        assert!(calls[0].samples[internal_start..internal_end]
            .iter()
            .all(|sample| *sample == 0));
    }

    #[tokio::test]
    async fn segment_duration_controls_max_batch_and_max_batch_does_not_end_utterance() {
        let (mut provider, processor, mut result_rx) = recording_provider(2.0).await;
        provider
            .feed_audio(pcm_chunk(pcm_frames(1_000, 100), 100, false))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();

        let calls = processor.calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].samples.len(), 2 * SAMPLE_RATE as usize);
        let partial = result_rx.recv().await.unwrap();
        let final_result = result_rx.recv().await.unwrap();
        assert!(!partial.is_final);
        assert!(final_result.is_final);
        assert!(result_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn endpoint_waits_for_final_response_then_sends_speech_before_silence() {
        let (processor, mut started_rx, mut releases) = controlled_processor(&[1]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor, "You", GROQ_BATCH_QUEUE_CAPACITY);
        let mut final_job = batch_job(1, GroqRequestType::Normal);
        final_job.ends_utterance = true;
        final_job.gate_reason = VoiceBatchReason::Endpoint;
        batch_tx.send(final_job).await.unwrap();
        drop(batch_tx);

        assert_eq!(started_rx.recv().await, Some(1));
        assert!(accumulator_rx.try_recv().is_err());
        releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("final speech")))
            .unwrap();

        assert_eq!(receive_speech(&mut accumulator_rx).await, "final speech");
        expect_silence(&mut accumulator_rx).await;
        assert!(worker.await.unwrap().is_ok());
        assert!(accumulator_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn rejected_final_response_produces_exactly_one_silence() {
        let processor = scripted_processor([(1, Ok(transcription(".")))]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor, "You", GROQ_BATCH_QUEUE_CAPACITY);
        let mut final_job = batch_job(1, GroqRequestType::Normal);
        final_job.ends_utterance = true;
        final_job.gate_reason = VoiceBatchReason::Endpoint;
        batch_tx.send(final_job).await.unwrap();
        drop(batch_tx);

        expect_silence(&mut accumulator_rx).await;
        assert!(worker.await.unwrap().is_ok());
        assert!(accumulator_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn failed_final_residual_closes_utterance_once() {
        let processor = scripted_processor([(1, Err("final residual failed".to_string()))]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor, "You", GROQ_BATCH_QUEUE_CAPACITY);
        let mut final_job = batch_job(1, GroqRequestType::Residual);
        final_job.ends_utterance = true;
        final_job.gate_reason = VoiceBatchReason::Finish;
        batch_tx.send(final_job).await.unwrap();
        drop(batch_tx);

        expect_silence(&mut accumulator_rx).await;
        assert_eq!(
            worker.await.unwrap().unwrap_err(),
            "final residual failed".to_string()
        );
        assert!(accumulator_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn secondary_rms_gate_blocks_ready_batch_without_calling_processor() {
        let processor = scripted_processor([]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor.clone(), "Them", GROQ_BATCH_QUEUE_CAPACITY);
        let mut quiet_ready_job = batch_job(1, GroqRequestType::Normal);
        quiet_ready_job.samples = vec![99; 960];
        quiet_ready_job.rms = 99.0;
        quiet_ready_job.gate_metrics = gate_metrics(960, 3);
        quiet_ready_job.ends_utterance = true;
        batch_tx.send(quiet_ready_job).await.unwrap();
        drop(batch_tx);

        expect_silence(&mut accumulator_rx).await;
        assert!(worker.await.unwrap().is_ok());
        assert!(processor.calls.lock().await.is_empty());
        assert!(accumulator_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn voice_gate_is_reset_between_streams() {
        let processor = recording_processor();
        let mut provider = GroqWhisperSTT::with_api_key("test-key");
        provider.batch_processor_override = Some(processor.clone());

        let (first_tx, mut first_rx) = mpsc::channel(4);
        provider.start_stream(first_tx).await.unwrap();
        provider
            .feed_audio(pcm_chunk(pcm_frames(1_000, 1), 100, false))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();
        assert!(first_rx.recv().await.is_none());

        let (second_tx, mut second_rx) = mpsc::channel(4);
        provider.start_stream(second_tx).await.unwrap();
        provider
            .feed_audio(pcm_chunk(pcm_frames(1_000, 2), 200, false))
            .await
            .unwrap();
        provider.stop_stream().await.unwrap();
        assert!(second_rx.recv().await.is_none());
        assert!(processor.calls.lock().await.is_empty());
        assert_eq!(provider.next_batch_sequence, 0);
    }

    #[tokio::test]
    async fn worker_processes_three_batches_fifo_with_one_active_request() {
        let (processor, mut started_rx, mut releases) = controlled_processor(&[1, 2, 3]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor.clone(), "You", GROQ_BATCH_QUEUE_CAPACITY);

        for sequence in 1..=3 {
            batch_tx
                .send(batch_job(sequence, GroqRequestType::Normal))
                .await
                .unwrap();
        }

        assert_eq!(started_rx.recv().await, Some(1));
        releases
            .remove(&2)
            .unwrap()
            .send(Ok(transcription("two")))
            .unwrap();
        tokio::task::yield_now().await;
        assert!(started_rx.try_recv().is_err());
        assert_eq!(processor.active.load(Ordering::SeqCst), 1);

        releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("one")))
            .unwrap();
        assert_eq!(receive_speech(&mut accumulator_rx).await, "one");
        assert_eq!(started_rx.recv().await, Some(2));
        assert_eq!(receive_speech(&mut accumulator_rx).await, "two");
        assert_eq!(started_rx.recv().await, Some(3));

        releases
            .remove(&3)
            .unwrap()
            .send(Ok(transcription("three")))
            .unwrap();
        assert_eq!(receive_speech(&mut accumulator_rx).await, "three");

        drop(batch_tx);
        assert!(worker.await.unwrap().is_ok());
        assert_eq!(processor.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(processor.active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_silence_cannot_overtake_an_active_http_batch() {
        let (processor, mut started_rx, mut releases) = controlled_processor(&[1]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor, "You", GROQ_BATCH_QUEUE_CAPACITY);
        batch_tx
            .send(batch_job(1, GroqRequestType::Normal))
            .await
            .unwrap();
        batch_tx.send(silent_batch_job(2)).await.unwrap();
        drop(batch_tx);

        assert_eq!(started_rx.recv().await, Some(1));
        tokio::task::yield_now().await;
        assert!(accumulator_rx.try_recv().is_err());
        releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("first")))
            .unwrap();

        assert_eq!(receive_speech(&mut accumulator_rx).await, "first");
        expect_silence(&mut accumulator_rx).await;
        assert!(worker.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn failed_batch_reports_silence_but_worker_processes_next_batch() {
        let (processor, mut started_rx, mut releases) = controlled_processor(&[1, 2]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor.clone(), "You", GROQ_BATCH_QUEUE_CAPACITY);
        batch_tx
            .send(batch_job(1, GroqRequestType::Normal))
            .await
            .unwrap();
        batch_tx
            .send(batch_job(2, GroqRequestType::Normal))
            .await
            .unwrap();
        drop(batch_tx);

        assert_eq!(started_rx.recv().await, Some(1));
        releases
            .remove(&1)
            .unwrap()
            .send(Err("mock first failure".to_string()))
            .unwrap();
        expect_silence(&mut accumulator_rx).await;
        assert_eq!(started_rx.recv().await, Some(2));
        releases
            .remove(&2)
            .unwrap()
            .send(Ok(transcription("second")))
            .unwrap();
        assert_eq!(receive_speech(&mut accumulator_rx).await, "second");

        let outcome = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker deadlocked")
            .unwrap();
        assert_eq!(outcome.unwrap_err(), "mock first failure");
        assert_eq!(processor.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_rejected_and_local_silence_keep_existing_accumulator_behavior() {
        let processor = scripted_processor([
            (1, Ok(transcription(""))),
            (2, Ok(transcription("."))),
            (4, Ok(transcription("valid speech"))),
        ]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor.clone(), "Them", GROQ_BATCH_QUEUE_CAPACITY);
        batch_tx
            .send(batch_job(1, GroqRequestType::Normal))
            .await
            .unwrap();
        batch_tx
            .send(batch_job(2, GroqRequestType::Normal))
            .await
            .unwrap();
        batch_tx.send(silent_batch_job(3)).await.unwrap();
        batch_tx
            .send(batch_job(4, GroqRequestType::Normal))
            .await
            .unwrap();
        drop(batch_tx);

        expect_silence(&mut accumulator_rx).await;
        expect_silence(&mut accumulator_rx).await;
        expect_silence(&mut accumulator_rx).await;
        assert_eq!(receive_speech(&mut accumulator_rx).await, "valid speech");
        assert!(worker.await.unwrap().is_ok());
        assert_eq!(*processor.calls.lock().await, vec![1, 2, 4]);
    }

    #[tokio::test]
    async fn residual_failure_preserves_the_existing_no_silence_behavior() {
        let processor = scripted_processor([(1, Err("residual failed".to_string()))]);
        let (batch_tx, mut accumulator_rx, worker) =
            spawn_test_worker(processor, "Them", GROQ_BATCH_QUEUE_CAPACITY);
        batch_tx
            .send(batch_job(1, GroqRequestType::Residual))
            .await
            .unwrap();
        drop(batch_tx);

        assert_eq!(worker.await.unwrap().unwrap_err(), "residual failed");
        assert!(accumulator_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn stop_drains_active_pending_and_residual_before_flush_and_return() {
        let (processor, mut started_rx, mut releases) = controlled_processor(&[1, 2, 3]);
        let mut provider = GroqWhisperSTT::with_api_key("test-key");
        provider.set_party("You");
        provider.set_config(GroqConfig {
            segment_duration_secs: 1.0,
            ..GroqConfig::default()
        });
        provider.batch_processor_override = Some(processor.clone());
        let (result_tx, mut result_rx) = mpsc::channel(16);
        provider.start_stream(result_tx).await.unwrap();

        provider.feed_audio(audio_chunk(16_000, 100)).await.unwrap();
        assert_eq!(started_rx.recv().await, Some(1));
        provider.feed_audio(audio_chunk(16_000, 200)).await.unwrap();
        provider.feed_audio(audio_chunk(8_000, 300)).await.unwrap();

        let mut stop_task = tokio::spawn(async move {
            let outcome = provider
                .stop_stream()
                .await
                .map_err(|error| error.to_string());
            (provider, outcome)
        });
        tokio::task::yield_now().await;
        assert!(matches!(
            futures::poll!(&mut stop_task),
            std::task::Poll::Pending
        ));

        releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("normal one")))
            .unwrap();
        assert_eq!(started_rx.recv().await, Some(2));
        releases
            .remove(&2)
            .unwrap()
            .send(Ok(transcription("normal two")))
            .unwrap();
        assert_eq!(started_rx.recv().await, Some(3));
        assert!(matches!(
            futures::poll!(&mut stop_task),
            std::task::Poll::Pending
        ));

        releases
            .remove(&3)
            .unwrap()
            .send(Ok(transcription("residual")))
            .unwrap();
        let (mut provider, outcome) = stop_task.await.unwrap();
        outcome.unwrap();

        let mut results = Vec::new();
        while let Some(result) = result_rx.recv().await {
            results.push(result);
        }
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].text, "normal one");
        assert_eq!(results[1].text, "normal one normal two");
        assert_eq!(results[2].text, "normal one normal two residual");
        assert!(!results[0].is_final);
        assert!(!results[1].is_final);
        assert!(!results[2].is_final);
        assert!(results[3].is_final);
        assert_eq!(results[3].text, results[2].text);
        assert!(provider.batch_tx.is_none());
        assert!(provider.batch_worker_task.is_none());
        assert!(!provider.is_streaming);
        assert_eq!(provider.next_batch_sequence, 3);
        assert_eq!(processor.max_active.load(Ordering::SeqCst), 1);

        provider.stop_stream().await.unwrap();
        assert_eq!(provider.next_batch_sequence, 3);
    }

    #[tokio::test]
    async fn stop_with_mock_error_drains_later_jobs_without_deadlock() {
        let processor = scripted_processor([
            (1, Err("mock network error".to_string())),
            (2, Ok(transcription("after failure"))),
        ]);
        let mut provider = GroqWhisperSTT::with_api_key("test-key");
        provider.set_config(GroqConfig {
            segment_duration_secs: 1.0,
            ..GroqConfig::default()
        });
        provider.batch_processor_override = Some(processor.clone());
        let (result_tx, mut result_rx) = mpsc::channel(8);
        provider.start_stream(result_tx).await.unwrap();
        provider.feed_audio(audio_chunk(16_000, 100)).await.unwrap();
        provider.feed_audio(audio_chunk(16_000, 200)).await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(1), provider.stop_stream())
            .await
            .expect("stop_stream deadlocked")
            .unwrap_err();
        assert_eq!(error.to_string(), "mock network error");
        assert_eq!(*processor.calls.lock().await, vec![1, 2]);
        assert!(provider.batch_worker_task.is_none());

        let partial = result_rx.recv().await.unwrap();
        let final_result = result_rx.recv().await.unwrap();
        assert_eq!(partial.text, "after failure");
        assert!(!partial.is_final);
        assert_eq!(final_result.text, "after failure");
        assert!(final_result.is_final);
        assert!(result_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn you_and_them_workers_are_independent_with_per_provider_sequences() {
        let (you_processor, mut you_started, mut you_releases) = controlled_processor(&[1, 2]);
        let (them_processor, mut them_started, mut them_releases) = controlled_processor(&[1, 2]);
        let config = GroqConfig {
            segment_duration_secs: 1.0,
            ..GroqConfig::default()
        };

        let mut you = GroqWhisperSTT::with_api_key("test-key");
        you.set_party("You");
        you.set_config(config.clone());
        you.batch_processor_override = Some(you_processor.clone());
        let (you_result_tx, mut you_results) = mpsc::channel(8);
        you.start_stream(you_result_tx).await.unwrap();

        let mut them = GroqWhisperSTT::with_api_key("test-key");
        them.set_party("Them");
        them.set_config(config);
        them.batch_processor_override = Some(them_processor.clone());
        let (them_result_tx, mut them_results) = mpsc::channel(8);
        them.start_stream(them_result_tx).await.unwrap();

        you.feed_audio(audio_chunk(16_000, 100)).await.unwrap();
        you.feed_audio(audio_chunk(16_000, 200)).await.unwrap();
        them.feed_audio(audio_chunk(16_000, 100)).await.unwrap();
        them.feed_audio(audio_chunk(16_000, 200)).await.unwrap();
        assert_eq!(you_started.recv().await, Some(1));
        assert_eq!(them_started.recv().await, Some(1));

        them_releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("them one")))
            .unwrap();
        assert_eq!(them_started.recv().await, Some(2));
        them_releases
            .remove(&2)
            .unwrap()
            .send(Ok(transcription("them two")))
            .unwrap();
        assert_eq!(receive_speech_result(&mut them_results).await, "them one");
        assert_eq!(
            receive_speech_result(&mut them_results).await,
            "them one them two"
        );
        assert!(you_results.try_recv().is_err());
        assert_eq!(you_processor.active.load(Ordering::SeqCst), 1);

        you_releases
            .remove(&1)
            .unwrap()
            .send(Ok(transcription("you one")))
            .unwrap();
        assert_eq!(you_started.recv().await, Some(2));
        you_releases
            .remove(&2)
            .unwrap()
            .send(Ok(transcription("you two")))
            .unwrap();
        assert_eq!(receive_speech_result(&mut you_results).await, "you one");
        assert_eq!(
            receive_speech_result(&mut you_results).await,
            "you one you two"
        );

        let (you_stop, them_stop) = tokio::join!(you.stop_stream(), them.stop_stream());
        you_stop.unwrap();
        them_stop.unwrap();
        assert_eq!(you.next_batch_sequence, 2);
        assert_eq!(them.next_batch_sequence, 2);
        assert_eq!(you_processor.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(them_processor.max_active.load(Ordering::SeqCst), 1);
    }

    async fn receive_speech_result(receiver: &mut mpsc::Receiver<TranscriptResult>) -> String {
        receiver.recv().await.unwrap().text
    }

    #[tokio::test]
    async fn bounded_queue_applies_backpressure_and_preserves_every_job() {
        let (batch_tx, mut batch_rx) = mpsc::channel(GROQ_BATCH_QUEUE_CAPACITY);
        assert_eq!(batch_tx.max_capacity(), GROQ_BATCH_QUEUE_CAPACITY);
        for sequence in 1..=GROQ_BATCH_QUEUE_CAPACITY as u64 {
            batch_tx
                .send(batch_job(sequence, GroqRequestType::Normal))
                .await
                .unwrap();
        }
        assert_eq!(batch_tx.capacity(), 0);

        let blocked_tx = batch_tx.clone();
        let (attempted_tx, attempted_rx) = oneshot::channel();
        let mut blocked_producer = tokio::spawn(async move {
            attempted_tx.send(()).unwrap();
            blocked_tx.send(batch_job(7, GroqRequestType::Normal)).await
        });
        attempted_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            futures::poll!(&mut blocked_producer),
            std::task::Poll::Pending
        ));

        assert_eq!(batch_rx.recv().await.unwrap().sequence, 1);
        blocked_producer.await.unwrap().unwrap();
        drop(batch_tx);

        let mut remaining = Vec::new();
        while let Some(job) = batch_rx.recv().await {
            remaining.push(job.sequence);
        }
        assert_eq!(remaining, vec![2, 3, 4, 5, 6, 7]);
    }

    #[tokio::test]
    async fn closing_full_queue_releases_blocked_producer_with_its_job() {
        let (batch_tx, batch_rx) = mpsc::channel(GROQ_BATCH_QUEUE_CAPACITY);
        for sequence in 1..=GROQ_BATCH_QUEUE_CAPACITY as u64 {
            batch_tx
                .send(batch_job(sequence, GroqRequestType::Normal))
                .await
                .unwrap();
        }

        let blocked_tx = batch_tx.clone();
        let (attempted_tx, attempted_rx) = oneshot::channel();
        let blocked_producer = tokio::spawn(async move {
            attempted_tx.send(()).unwrap();
            blocked_tx.send(batch_job(7, GroqRequestType::Normal)).await
        });
        attempted_rx.await.unwrap();
        tokio::task::yield_now().await;
        drop(batch_rx);

        let error = blocked_producer.await.unwrap().unwrap_err();
        assert_eq!(error.0.sequence, 7);
        drop(batch_tx);
    }

    #[tokio::test]
    async fn silence_finalizes_an_open_partial_with_derived_confidence() {
        let mut provider = GroqWhisperSTT::with_api_key("test-key");
        let (result_tx, mut result_rx) = mpsc::channel(4);
        provider.start_stream(result_tx).await.unwrap();

        provider
            .accumulator_tx
            .as_ref()
            .unwrap()
            .send(AccumulatorMsg::Speech {
                text: "sim".to_string(),
                confidence: Some(0.73),
                timestamp_ms: 42,
            })
            .await
            .unwrap();

        let partial = result_rx.recv().await.unwrap();
        assert!(!partial.is_final);
        assert_eq!(partial.confidence, 0.73);

        provider
            .accumulator_tx
            .as_ref()
            .unwrap()
            .send(AccumulatorMsg::Silence)
            .await
            .unwrap();

        let final_result = result_rx.recv().await.unwrap();
        assert!(final_result.is_final);
        assert_eq!(final_result.text, "sim");
        assert_eq!(final_result.confidence, 0.73);
        assert!(result_rx.try_recv().is_err());

        tokio::time::timeout(Duration::from_secs(1), provider.stop_stream())
            .await
            .expect("empty worker deadlocked during stop")
            .unwrap();
        assert!(provider.batch_tx.is_none());
        assert!(provider.batch_worker_task.is_none());

        tokio::time::timeout(Duration::from_secs(1), provider.stop_stream())
            .await
            .expect("repeated stop deadlocked")
            .unwrap();
        assert!(result_rx.try_recv().is_err());
    }
}
