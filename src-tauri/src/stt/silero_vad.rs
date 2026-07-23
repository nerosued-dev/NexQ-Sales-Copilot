//! Stateful Silero VAD inference for 16 kHz mono PCM.
//!
//! The embedded model is the official Silero VAD v6.2.1 ONNX artifact. This
//! module only classifies fixed acoustic frames; endpointing and Groq batching
//! remain in [`crate::stt::local_voice_gate`].

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use ndarray::{Array0, Array2, Array3};
use ort::session::Session;
use ort::value::{Tensor, TensorElementType, ValueType};

use crate::stt::local_voice_gate::PcmFrame;

pub const SILERO_VAD_VERSION: &str = "6.2.1";
pub const SILERO_VAD_SOURCE: &str =
    "https://github.com/snakers4/silero-vad/blob/v6.2.1/src/silero_vad/data/silero_vad.onnx";
pub const SILERO_VAD_SHA256: &str =
    "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3";
pub const SILERO_VAD_ONNX_OPSET: u32 = 16;
pub const SILERO_SAMPLE_RATE_HZ: u32 = 16_000;
pub const SILERO_FRAME_SAMPLES: usize = 512;
pub const SILERO_FRAME_DURATION: Duration = Duration::from_millis(32);
const SILERO_CONTEXT_SAMPLES: usize = 64;
const SILERO_STATE_LAYERS: usize = 2;
const SILERO_STATE_SIZE: usize = 128;

const EMBEDDED_MODEL: &[u8] = include_bytes!("../../resources/silero-vad/v6.2.1/silero_vad.onnx");
const _SILERO_LICENSE: &str = include_str!("../../resources/silero-vad/v6.2.1/LICENSE");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SileroTensorSignature {
    pub name: String,
    pub element_type: TensorElementType,
    pub dimensions: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SileroModelSignature {
    pub inputs: Vec<SileroTensorSignature>,
    pub outputs: Vec<SileroTensorSignature>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SileroVadFrame {
    pub pcm: PcmFrame,
    pub speech_probability: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SileroVadError {
    ModelLoad,
    InvalidModelSignature(&'static str),
    InvalidFrameSize { actual: usize },
    Inference,
    InvalidModelOutput(&'static str),
}

impl fmt::Display for SileroVadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelLoad => formatter.write_str("Silero VAD model could not be loaded"),
            Self::InvalidModelSignature(message) => {
                write!(formatter, "Silero VAD model signature is invalid: {message}")
            }
            Self::InvalidFrameSize { actual } => write!(
                formatter,
                "Silero VAD requires exactly {SILERO_FRAME_SAMPLES} samples per frame; received {actual}"
            ),
            Self::Inference => formatter.write_str("Silero VAD inference failed"),
            Self::InvalidModelOutput(message) => {
                write!(formatter, "Silero VAD returned invalid output: {message}")
            }
        }
    }
}

impl std::error::Error for SileroVadError {}

/// Per-stream Silero classifier.
///
/// The ONNX session, recurrent state, 64-sample context, and incomplete PCM
/// frame are owned by one instance. A provider creates one instance per stream,
/// keeping You and Them fully independent.
pub struct SileroVadClassifier {
    session: Session,
    signature: SileroModelSignature,
    output_index: usize,
    next_state_index: usize,
    state: Array3<f32>,
    context: [f32; SILERO_CONTEXT_SAMPLES],
    remainder: Vec<i16>,
    emitted_samples: u64,
    accepted_samples: u64,
}

impl fmt::Debug for SileroVadClassifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SileroVadClassifier")
            .field("version", &SILERO_VAD_VERSION)
            .field("remainder_samples", &self.remainder.len())
            .field("emitted_samples", &self.emitted_samples)
            .field("accepted_samples", &self.accepted_samples)
            .finish_non_exhaustive()
    }
}

impl SileroVadClassifier {
    pub fn from_embedded_model() -> Result<Self, SileroVadError> {
        Self::from_model_bytes(EMBEDDED_MODEL)
    }

    fn from_model_bytes(model: &[u8]) -> Result<Self, SileroVadError> {
        let mut builder = Session::builder().map_err(|_| SileroVadError::ModelLoad)?;
        builder = builder
            .with_execution_providers([ort::ep::CPU::default().with_arena_allocator(true).build()])
            .map_err(|_| SileroVadError::ModelLoad)?
            .with_intra_threads(1)
            .map_err(|_| SileroVadError::ModelLoad)?
            .with_inter_threads(1)
            .map_err(|_| SileroVadError::ModelLoad)?;
        let session = builder
            .commit_from_memory(model)
            .map_err(|_| SileroVadError::ModelLoad)?;
        let (signature, output_index, next_state_index) = inspect_and_validate_signature(&session)?;

        Ok(Self {
            session,
            signature,
            output_index,
            next_state_index,
            state: Array3::zeros((SILERO_STATE_LAYERS, 1, SILERO_STATE_SIZE)),
            context: [0.0; SILERO_CONTEXT_SAMPLES],
            remainder: Vec::with_capacity(SILERO_FRAME_SAMPLES),
            emitted_samples: 0,
            accepted_samples: 0,
        })
    }

    pub fn signature(&self) -> &SileroModelSignature {
        &self.signature
    }

    /// Accept variable callback sizes and classify every complete 512-sample
    /// frame in chronological order.
    pub fn push_samples(&mut self, samples: &[i16]) -> Result<Vec<SileroVadFrame>, SileroVadError> {
        self.accepted_samples = self.accepted_samples.saturating_add(samples.len() as u64);
        self.remainder.extend_from_slice(samples);

        let complete_samples = self.remainder.len() / SILERO_FRAME_SAMPLES * SILERO_FRAME_SAMPLES;
        if complete_samples == 0 {
            return Ok(Vec::new());
        }

        let pending = std::mem::take(&mut self.remainder);
        let frame_count = complete_samples / SILERO_FRAME_SAMPLES;
        let mut frames = Vec::with_capacity(frame_count);
        for samples in pending[..complete_samples].chunks_exact(SILERO_FRAME_SAMPLES) {
            let start_sample = self.emitted_samples;
            let probability = self.classify_frame(samples)?;
            frames.push(SileroVadFrame {
                pcm: PcmFrame {
                    samples: samples.to_vec(),
                    valid_samples: SILERO_FRAME_SAMPLES,
                    start_sample,
                },
                speech_probability: probability,
            });
            self.emitted_samples = self
                .emitted_samples
                .saturating_add(SILERO_FRAME_SAMPLES as u64);
        }
        self.remainder
            .extend_from_slice(&pending[complete_samples..]);
        Ok(frames)
    }

    /// Classify the final partial frame using the official zero-padding
    /// behavior. Only real samples are retained in the returned PCM frame.
    pub fn finish(&mut self) -> Result<Vec<SileroVadFrame>, SileroVadError> {
        if self.remainder.is_empty() {
            return Ok(Vec::new());
        }

        let real_samples = std::mem::take(&mut self.remainder);
        let valid_samples = real_samples.len();
        let mut padded = real_samples.clone();
        padded.resize(SILERO_FRAME_SAMPLES, 0);
        let start_sample = self.emitted_samples;
        let probability = self.classify_frame(&padded)?;
        self.emitted_samples = self.emitted_samples.saturating_add(valid_samples as u64);

        Ok(vec![SileroVadFrame {
            pcm: PcmFrame {
                samples: real_samples,
                valid_samples,
                start_sample,
            },
            speech_probability: probability,
        }])
    }

    /// Reset all state that can carry evidence across streams.
    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
        self.remainder.clear();
        self.emitted_samples = 0;
        self.accepted_samples = 0;
    }

    pub fn remainder_len(&self) -> usize {
        self.remainder.len()
    }

    pub fn timeline_samples(&self) -> u64 {
        self.accepted_samples
    }

    /// Run one exact official 16 kHz frame, preserving recurrent state.
    pub fn classify_frame(&mut self, samples: &[i16]) -> Result<f32, SileroVadError> {
        if samples.len() != SILERO_FRAME_SAMPLES {
            return Err(SileroVadError::InvalidFrameSize {
                actual: samples.len(),
            });
        }

        let normalized = samples.iter().map(|&sample| f32::from(sample) / 32_768.0);
        let model_input = self
            .context
            .iter()
            .copied()
            .chain(normalized)
            .collect::<Vec<_>>();
        let input_array = Array2::from_shape_vec(
            (1, SILERO_CONTEXT_SAMPLES + SILERO_FRAME_SAMPLES),
            model_input,
        )
        .map_err(|_| SileroVadError::Inference)?;
        let state_array = self.state.clone();
        let sample_rate = Array0::from_elem((), i64::from(SILERO_SAMPLE_RATE_HZ));

        let input_tensor =
            Tensor::from_array(input_array).map_err(|_| SileroVadError::Inference)?;
        let state_tensor =
            Tensor::from_array(state_array).map_err(|_| SileroVadError::Inference)?;
        let sample_rate_tensor =
            Tensor::from_array(sample_rate).map_err(|_| SileroVadError::Inference)?;
        let inputs: Vec<(Cow<'_, str>, ort::session::SessionInputValue<'_>)> = vec![
            ("input".into(), input_tensor.into()),
            ("state".into(), state_tensor.into()),
            ("sr".into(), sample_rate_tensor.into()),
        ];

        let outputs = self
            .session
            .run(inputs)
            .map_err(|_| SileroVadError::Inference)?;
        let (_, probability_data) = outputs[self.output_index]
            .try_extract_tensor::<f32>()
            .map_err(|_| SileroVadError::InvalidModelOutput("output must be float32"))?;
        let probability =
            probability_data
                .first()
                .copied()
                .ok_or(SileroVadError::InvalidModelOutput(
                    "output tensor must contain one probability",
                ))?;
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(SileroVadError::InvalidModelOutput(
                "probability must be finite and within 0..=1",
            ));
        }

        let (state_shape, state_data) = outputs[self.next_state_index]
            .try_extract_tensor::<f32>()
            .map_err(|_| SileroVadError::InvalidModelOutput("stateN must be float32"))?;
        let state_dimensions = state_shape
            .iter()
            .map(|&dimension| dimension as usize)
            .collect::<Vec<_>>();
        if state_dimensions != [SILERO_STATE_LAYERS, 1, SILERO_STATE_SIZE] {
            return Err(SileroVadError::InvalidModelOutput(
                "stateN must have shape [2, 1, 128]",
            ));
        }
        self.state = Array3::from_shape_vec(
            (SILERO_STATE_LAYERS, 1, SILERO_STATE_SIZE),
            state_data.to_vec(),
        )
        .map_err(|_| SileroVadError::InvalidModelOutput("stateN shape is inconsistent"))?;

        for (destination, &sample) in self
            .context
            .iter_mut()
            .zip(samples[SILERO_FRAME_SAMPLES - SILERO_CONTEXT_SAMPLES..].iter())
        {
            *destination = f32::from(sample) / 32_768.0;
        }

        Ok(probability)
    }

    #[cfg(test)]
    fn recurrent_state(&self) -> &[f32] {
        self.state
            .as_slice()
            .expect("classifier state is contiguous")
    }

    #[cfg(test)]
    fn context(&self) -> &[f32; SILERO_CONTEXT_SAMPLES] {
        &self.context
    }
}

fn inspect_and_validate_signature(
    session: &Session,
) -> Result<(SileroModelSignature, usize, usize), SileroVadError> {
    let inputs = session
        .inputs()
        .iter()
        .map(|input| tensor_signature(input.name(), input.dtype()))
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = session
        .outputs()
        .iter()
        .map(|output| tensor_signature(output.name(), output.dtype()))
        .collect::<Result<Vec<_>, _>>()?;

    validate_tensor(&inputs, "input", TensorElementType::Float32, 2)?;
    let state = validate_tensor(&inputs, "state", TensorElementType::Float32, 3)?;
    if state.dimensions.first().copied() != Some(SILERO_STATE_LAYERS as i64)
        || state.dimensions.last().copied() != Some(SILERO_STATE_SIZE as i64)
    {
        return Err(SileroVadError::InvalidModelSignature(
            "state must have shape [2, batch, 128]",
        ));
    }
    validate_tensor(&inputs, "sr", TensorElementType::Int64, 0)?;
    validate_tensor(&outputs, "output", TensorElementType::Float32, 2)?;
    validate_tensor(&outputs, "stateN", TensorElementType::Float32, 3)?;

    if inputs.len() != 3 || outputs.len() != 2 {
        return Err(SileroVadError::InvalidModelSignature(
            "expected exactly 3 inputs and 2 outputs",
        ));
    }

    let output_index = outputs
        .iter()
        .position(|tensor| tensor.name == "output")
        .ok_or(SileroVadError::InvalidModelSignature(
            "missing output tensor",
        ))?;
    let next_state_index = outputs
        .iter()
        .position(|tensor| tensor.name == "stateN")
        .ok_or(SileroVadError::InvalidModelSignature(
            "missing stateN tensor",
        ))?;

    Ok((
        SileroModelSignature { inputs, outputs },
        output_index,
        next_state_index,
    ))
}

fn tensor_signature(
    name: &str,
    value_type: &ValueType,
) -> Result<SileroTensorSignature, SileroVadError> {
    let ValueType::Tensor {
        ty,
        shape,
        dimension_symbols: _,
    } = value_type
    else {
        return Err(SileroVadError::InvalidModelSignature(
            "all inputs and outputs must be tensors",
        ));
    };

    Ok(SileroTensorSignature {
        name: name.to_string(),
        element_type: *ty,
        dimensions: shape.iter().copied().collect(),
    })
}

fn validate_tensor<'a>(
    tensors: &'a [SileroTensorSignature],
    name: &'static str,
    element_type: TensorElementType,
    rank: usize,
) -> Result<&'a SileroTensorSignature, SileroVadError> {
    let tensor = tensors.iter().find(|tensor| tensor.name == name).ok_or(
        SileroVadError::InvalidModelSignature("required tensor is missing"),
    )?;
    if tensor.element_type != element_type {
        return Err(SileroVadError::InvalidModelSignature(
            "tensor element type is incompatible",
        ));
    }
    if tensor.dimensions.len() != rank {
        return Err(SileroVadError::InvalidModelSignature(
            "tensor rank is incompatible",
        ));
    }
    Ok(tensor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn current_process_working_set_bytes() -> Option<u64> {
        let command = format!("(Get-Process -Id {}).WorkingSet64", std::process::id());
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()?.trim().parse().ok()
    }

    #[cfg(not(windows))]
    fn current_process_working_set_bytes() -> Option<u64> {
        None
    }

    fn classifier() -> SileroVadClassifier {
        SileroVadClassifier::from_embedded_model().expect("official embedded model must load")
    }

    fn deterministic_noise(sample_count: usize, amplitude: i16) -> Vec<i16> {
        let mut state = 0x1234_5678_u32;
        (0..sample_count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let centered = ((state >> 16) as i32 - 32_768) as f32 / 32_768.0;
                (centered * f32::from(amplitude)) as i16
            })
            .collect()
    }

    fn sine_wave(sample_count: usize, frequency_hz: f32, amplitude: f32) -> Vec<i16> {
        (0..sample_count)
            .map(|index| {
                let phase = std::f32::consts::TAU * frequency_hz * index as f32
                    / SILERO_SAMPLE_RATE_HZ as f32;
                (phase.sin() * amplitude) as i16
            })
            .collect()
    }

    fn speech_fixture_pcm() -> Vec<i16> {
        let bytes = include_bytes!("fixtures/silero-vad/pt-BR-sim.wav");
        let reader = hound::WavReader::new(std::io::Cursor::new(bytes))
            .expect("generated speech fixture must be valid WAV");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, SILERO_SAMPLE_RATE_HZ);
        assert_eq!(spec.bits_per_sample, 16);
        reader
            .into_samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .expect("generated speech fixture PCM")
    }

    #[test]
    fn official_embedded_model_loads_and_has_expected_signature() {
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(EMBEDDED_MODEL);
        assert_eq!(format!("{digest:x}"), SILERO_VAD_SHA256);
        assert!(_SILERO_LICENSE.starts_with("MIT License"));

        let classifier = classifier();
        let signature = classifier.signature();
        assert_eq!(
            signature
                .inputs
                .iter()
                .map(|tensor| tensor.name.as_str())
                .collect::<Vec<_>>(),
            ["input", "state", "sr"]
        );
        assert_eq!(
            signature
                .outputs
                .iter()
                .map(|tensor| tensor.name.as_str())
                .collect::<Vec<_>>(),
            ["output", "stateN"]
        );
        assert_eq!(
            signature
                .inputs
                .iter()
                .find(|tensor| tensor.name == "state")
                .expect("state input")
                .dimensions,
            [2, -1, 128]
        );
    }

    #[test]
    fn invalid_model_bytes_return_sanitized_typed_error() {
        assert!(matches!(
            SileroVadClassifier::from_model_bytes(b"not an ONNX model"),
            Err(SileroVadError::ModelLoad)
        ));
    }

    #[test]
    fn exact_frame_is_accepted_and_wrong_size_is_controlled_error() {
        let mut classifier = classifier();
        let probability = classifier
            .classify_frame(&[0; SILERO_FRAME_SAMPLES])
            .expect("512 samples must be accepted");
        assert!((0.0..=1.0).contains(&probability));

        assert_eq!(
            classifier.classify_frame(&[0; SILERO_FRAME_SAMPLES - 1]),
            Err(SileroVadError::InvalidFrameSize {
                actual: SILERO_FRAME_SAMPLES - 1
            })
        );
    }

    #[test]
    fn absolute_silence_and_low_white_noise_have_low_probability() {
        let mut silence_classifier = classifier();
        let silence = silence_classifier
            .classify_frame(&[0; SILERO_FRAME_SAMPLES])
            .expect("silence inference");
        assert!(silence < 0.1, "silence probability was {silence}");

        let mut noise_classifier = classifier();
        let noise = noise_classifier
            .classify_frame(&deterministic_noise(SILERO_FRAME_SAMPLES, 100))
            .expect("noise inference");
        assert!(
            noise < 0.35,
            "noise probability crossed the negative hysteresis threshold: {noise}"
        );
    }

    #[test]
    fn impulse_and_tone_do_not_create_sustained_positive_run() {
        let mut impulse_classifier = classifier();
        let mut impulse = vec![0; SILERO_FRAME_SAMPLES * 6];
        impulse[SILERO_FRAME_SAMPLES / 2] = i16::MAX;
        let impulse_frames = impulse_classifier
            .push_samples(&impulse)
            .expect("impulse inference");
        assert!(
            impulse_frames
                .iter()
                .filter(|frame| frame.speech_probability >= 0.5)
                .count()
                < 2
        );

        let mut tone_classifier = classifier();
        let tone = sine_wave(SILERO_FRAME_SAMPLES * 12, 1_000.0, 8_000.0);
        let tone_frames = tone_classifier.push_samples(&tone).expect("tone inference");
        let longest_run = tone_frames
            .iter()
            .fold((0_usize, 0_usize), |(current, longest), frame| {
                if frame.speech_probability >= 0.5 {
                    let current = current + 1;
                    (current, longest.max(current))
                } else {
                    (0, longest)
                }
            })
            .1;
        assert!(longest_run < 2, "tone positive run was {longest_run}");
    }

    #[test]
    fn generated_short_speech_has_confirmable_positive_run() {
        let mut classifier = classifier();
        let frames = classifier
            .push_samples(&speech_fixture_pcm())
            .expect("speech fixture inference");
        let longest_run = frames
            .iter()
            .fold((0_usize, 0_usize), |(current, longest), frame| {
                if frame.speech_probability >= 0.5 {
                    let current = current + 1;
                    (current, longest.max(current))
                } else {
                    (0, longest)
                }
            })
            .1;
        assert!(
            longest_run >= 2,
            "speech fixture longest positive run was {longest_run}"
        );
    }

    #[test]
    fn recurrent_state_is_preserved_and_reset_clears_every_stream_field() {
        let mut classifier = classifier();
        classifier
            .push_samples(&sine_wave(SILERO_FRAME_SAMPLES, 220.0, 4_000.0))
            .expect("stateful inference");
        assert!(classifier
            .recurrent_state()
            .iter()
            .any(|value| *value != 0.0));
        assert!(classifier.context().iter().any(|value| *value != 0.0));

        classifier.push_samples(&[1, 2, 3]).expect("remainder");
        classifier.reset();
        assert!(classifier
            .recurrent_state()
            .iter()
            .all(|value| *value == 0.0));
        assert!(classifier.context().iter().all(|value| *value == 0.0));
        assert_eq!(classifier.remainder_len(), 0);
        assert_eq!(classifier.timeline_samples(), 0);
    }

    #[test]
    fn instances_do_not_share_recurrent_state() {
        let mut first = classifier();
        let second = classifier();
        first
            .classify_frame(&sine_wave(SILERO_FRAME_SAMPLES, 220.0, 4_000.0))
            .expect("first inference");
        assert!(first.recurrent_state().iter().any(|value| *value != 0.0));
        assert!(second.recurrent_state().iter().all(|value| *value == 0.0));
    }

    #[test]
    fn splitter_preserves_remainder_timeline_and_partial_valid_samples() {
        let mut classifier = classifier();
        let input = deterministic_noise(SILERO_FRAME_SAMPLES * 2 + 17, 50);
        let mut frames = Vec::new();
        frames.extend(
            classifier
                .push_samples(&input[..123])
                .expect("first callback"),
        );
        frames.extend(
            classifier
                .push_samples(&input[123..900])
                .expect("second callback"),
        );
        frames.extend(
            classifier
                .push_samples(&input[900..])
                .expect("third callback"),
        );
        frames.extend(classifier.finish().expect("final partial frame"));

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].pcm.start_sample, 0);
        assert_eq!(frames[1].pcm.start_sample, SILERO_FRAME_SAMPLES as u64);
        assert_eq!(
            frames[2].pcm.start_sample,
            (SILERO_FRAME_SAMPLES * 2) as u64
        );
        assert_eq!(frames[2].pcm.valid_samples, 17);
        let reconstructed = frames
            .into_iter()
            .flat_map(|frame| frame.pcm.samples)
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, input);
        assert_eq!(classifier.timeline_samples(), input.len() as u64);
    }

    #[test]
    #[ignore = "local diagnostic benchmark; run explicitly with --ignored --nocapture"]
    fn benchmark_single_thread_inference() {
        let working_set_before = current_process_working_set_bytes();
        let init_started = std::time::Instant::now();
        let mut classifier = classifier();
        let initialization = init_started.elapsed();
        let working_set_after = current_process_working_set_bytes();
        let frame = deterministic_noise(SILERO_FRAME_SAMPLES, 100);
        let mut timings = Vec::with_capacity(1_000);

        for _ in 0..1_000 {
            let started = std::time::Instant::now();
            classifier
                .classify_frame(&frame)
                .expect("benchmark inference");
            timings.push(started.elapsed());
        }
        timings.sort_unstable();
        let total_nanos = timings
            .iter()
            .map(std::time::Duration::as_nanos)
            .sum::<u128>();
        let mean = Duration::from_nanos((total_nanos / timings.len() as u128) as u64);
        let p95 = timings[timings.len() * 95 / 100];
        let frames_per_second = 1.0 / mean.as_secs_f64();
        let model_and_state_bytes = EMBEDDED_MODEL.len()
            + std::mem::size_of_val(classifier.recurrent_state())
            + std::mem::size_of_val(classifier.context());
        let session_working_set_delta_bytes = working_set_before
            .zip(working_set_after)
            .map(|(before, after)| after.saturating_sub(before));

        eprintln!(
            "silero_vad init_ms={:.3} mean_ms={:.3} p95_ms={:.3} frames_per_second={:.1} embedded_model_and_state_bytes={} session_working_set_delta_bytes={:?} frame_audio_ms=32",
            initialization.as_secs_f64() * 1_000.0,
            mean.as_secs_f64() * 1_000.0,
            p95.as_secs_f64() * 1_000.0,
            frames_per_second,
            model_and_state_bytes,
            session_working_set_delta_bytes
        );
    }
}
