//! Pure, deterministic endpointing for classified 16 kHz mono PCM frames.
//!
//! The gate does not inspect PCM energy and does not run inference. Production
//! frames arrive with probabilities from Silero VAD; tests can inject explicit
//! probabilities without loading ONNX Runtime.

use std::collections::VecDeque;
use std::fmt;

pub const SAMPLE_RATE_HZ: u32 = 16_000;
pub const FRAME_DURATION_MS: u32 = 32;
pub const FRAME_SAMPLES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmFrame {
    pub samples: Vec<i16>,
    pub valid_samples: usize,
    pub start_sample: u64,
}

/// Initial backend-only Silero and endpointing hypotheses.
///
/// These values are intentionally explicit and are not considered calibrated:
/// enter at 0.50, leave below 0.35, confirm two positive frames, keep 320 ms
/// pre-roll, endpoint after 704 ms, and retain 192 ms post-roll.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalVoiceGateConfig {
    pub speech_threshold: f32,
    pub negative_threshold: f32,
    pub pre_roll_ms: u32,
    pub activation_window_ms: u32,
    pub min_positive_ms: u32,
    pub min_consecutive_positive_frames: usize,
    pub end_silence_ms: u32,
    pub post_roll_ms: u32,
    pub max_batch_ms: u32,
}

impl LocalVoiceGateConfig {
    pub fn initial_test_hypothesis() -> Self {
        Self {
            speech_threshold: 0.50,
            negative_threshold: 0.35,
            pre_roll_ms: 320,
            activation_window_ms: 160,
            min_positive_ms: 64,
            min_consecutive_positive_frames: 2,
            end_silence_ms: 704,
            post_roll_ms: 192,
            max_batch_ms: 4_992,
        }
    }

    fn validate(&self) -> Result<DerivedConfig, LocalVoiceGateConfigError> {
        if !self.speech_threshold.is_finite() || !(0.0..=1.0).contains(&self.speech_threshold) {
            return Err(LocalVoiceGateConfigError::new(
                "speech_threshold must be finite and within 0..=1",
            ));
        }
        if !self.negative_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.negative_threshold)
            || self.negative_threshold >= self.speech_threshold
        {
            return Err(LocalVoiceGateConfigError::new(
                "negative_threshold must be finite, within 0..=1, and below speech_threshold",
            ));
        }

        for (name, duration) in [
            ("pre_roll_ms", self.pre_roll_ms),
            ("activation_window_ms", self.activation_window_ms),
            ("min_positive_ms", self.min_positive_ms),
            ("end_silence_ms", self.end_silence_ms),
            ("post_roll_ms", self.post_roll_ms),
            ("max_batch_ms", self.max_batch_ms),
        ] {
            if duration % FRAME_DURATION_MS != 0 {
                let message = match name {
                    "pre_roll_ms" => "pre_roll_ms must align to 32 ms Silero frames",
                    "activation_window_ms" => {
                        "activation_window_ms must align to 32 ms Silero frames"
                    }
                    "min_positive_ms" => "min_positive_ms must align to 32 ms Silero frames",
                    "end_silence_ms" => "end_silence_ms must align to 32 ms Silero frames",
                    "post_roll_ms" => "post_roll_ms must align to 32 ms Silero frames",
                    _ => "max_batch_ms must align to 32 ms Silero frames",
                };
                return Err(LocalVoiceGateConfigError::new(message));
            }
        }

        if self.activation_window_ms == 0
            || self.min_positive_ms == 0
            || self.min_consecutive_positive_frames == 0
            || self.end_silence_ms == 0
            || self.max_batch_ms == 0
        {
            return Err(LocalVoiceGateConfigError::new(
                "activation, speech, endpoint, and batch limits must be non-zero",
            ));
        }
        if self.min_positive_ms > self.activation_window_ms {
            return Err(LocalVoiceGateConfigError::new(
                "min_positive_ms cannot exceed activation_window_ms",
            ));
        }
        if self.min_consecutive_positive_frames as u32 * FRAME_DURATION_MS
            > self.activation_window_ms
        {
            return Err(LocalVoiceGateConfigError::new(
                "minimum consecutive evidence cannot exceed the activation window",
            ));
        }
        if self.post_roll_ms > self.end_silence_ms {
            return Err(LocalVoiceGateConfigError::new(
                "post_roll_ms cannot exceed end_silence_ms",
            ));
        }

        Ok(DerivedConfig {
            speech_threshold: self.speech_threshold,
            negative_threshold: self.negative_threshold,
            pre_roll_frames: frames_for_ms(self.pre_roll_ms),
            activation_window_samples: samples_for_ms(self.activation_window_ms),
            min_positive_samples: samples_for_ms(self.min_positive_ms),
            min_consecutive_positive_samples: self.min_consecutive_positive_frames * FRAME_SAMPLES,
            end_silence_samples: samples_for_ms(self.end_silence_ms),
            post_roll_frames: frames_for_ms(self.post_roll_ms),
            max_batch_frames: frames_for_ms(self.max_batch_ms),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVoiceGateConfigError {
    message: &'static str,
}

impl LocalVoiceGateConfigError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for LocalVoiceGateConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for LocalVoiceGateConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalVoiceGateInputError {
    EmptyFrame,
    InvalidValidSampleCount,
    InvalidProbability,
    DiscontinuousTimeline { expected: u64, actual: u64 },
}

impl fmt::Display for LocalVoiceGateInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFrame => formatter.write_str("voice evidence frame cannot be empty"),
            Self::InvalidValidSampleCount => {
                formatter.write_str("voice evidence valid sample count is invalid")
            }
            Self::InvalidProbability => {
                formatter.write_str("voice probability must be finite and within 0..=1")
            }
            Self::DiscontinuousTimeline { expected, actual } => write!(
                formatter,
                "voice evidence timeline is discontinuous: expected sample {expected}, received {actual}"
            ),
        }
    }
}

impl std::error::Error for LocalVoiceGateInputError {}

#[derive(Debug, Clone, Copy)]
struct DerivedConfig {
    speech_threshold: f32,
    negative_threshold: f32,
    pre_roll_frames: usize,
    activation_window_samples: usize,
    min_positive_samples: usize,
    min_consecutive_positive_samples: usize,
    end_silence_samples: usize,
    post_roll_frames: usize,
    max_batch_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceGateState {
    Idle,
    Candidate,
    Active,
    Hangover,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceBatchMetrics {
    pub total_frames: usize,
    pub positive_frames: usize,
    pub positive_ratio: f32,
    pub mean_probability: f32,
    pub max_probability: f32,
    pub positive_samples: usize,
    pub positive_duration_ms: f64,
    pub longest_positive_run_frames: usize,
    pub longest_positive_run_ms: f64,
    pub longest_silence_run_frames: usize,
    pub longest_silence_run_ms: f64,
    pub real_samples: usize,
    pub total_duration_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadyBatch {
    pub samples: Vec<i16>,
    pub ends_utterance: bool,
    pub metrics: VoiceBatchMetrics,
}

#[derive(Debug, Clone)]
struct ClassifiedFrame {
    frame: PcmFrame,
    probability: f32,
    positive: bool,
}

/// Deterministic endpointing state machine for one audio stream.
#[derive(Debug)]
pub struct LocalVoiceGate {
    config: DerivedConfig,
    state: VoiceGateState,
    pre_roll: VecDeque<ClassifiedFrame>,
    candidate_audio: Vec<ClassifiedFrame>,
    candidate_evidence: Vec<ClassifiedFrame>,
    active_audio: Vec<ClassifiedFrame>,
    trailing_silence_samples: usize,
    next_sample: u64,
}

impl LocalVoiceGate {
    pub fn new(config: LocalVoiceGateConfig) -> Result<Self, LocalVoiceGateConfigError> {
        Ok(Self {
            config: config.validate()?,
            state: VoiceGateState::Idle,
            pre_roll: VecDeque::new(),
            candidate_audio: Vec::new(),
            candidate_evidence: Vec::new(),
            active_audio: Vec::new(),
            trailing_silence_samples: 0,
            next_sample: 0,
        })
    }

    /// Consume one chronological Silero frame. Activation uses only the 0.50
    /// speech threshold. Once active, the 0.35 negative threshold supplies the
    /// same neutral hysteresis band used by Silero's official iterator.
    pub fn push_evidence(
        &mut self,
        frame: PcmFrame,
        speech_probability: f32,
    ) -> Result<Vec<ReadyBatch>, LocalVoiceGateInputError> {
        self.validate_frame(&frame, speech_probability)?;
        self.next_sample = self.next_sample.saturating_add(frame.valid_samples as u64);

        let positive = speech_probability >= self.config.speech_threshold;
        let classified = ClassifiedFrame {
            frame,
            probability: speech_probability,
            positive,
        };

        let batches = match self.state {
            VoiceGateState::Idle => {
                self.process_idle(classified);
                Vec::new()
            }
            VoiceGateState::Candidate => {
                self.process_candidate(classified);
                if self.state == VoiceGateState::Active {
                    self.take_max_batch()
                } else {
                    Vec::new()
                }
            }
            VoiceGateState::Active | VoiceGateState::Hangover => self.process_active(classified),
        };
        Ok(batches)
    }

    /// Finalize only previously confirmed speech. Unconfirmed candidates are
    /// discarded, and no PCM classification occurs here.
    pub fn finish(&mut self) -> Vec<ReadyBatch> {
        let mut batches = Vec::new();
        match self.state {
            VoiceGateState::Active | VoiceGateState::Hangover => {
                if !self.active_audio.is_empty() {
                    batches.push(make_batch(std::mem::take(&mut self.active_audio), true));
                }
            }
            VoiceGateState::Candidate => {
                self.candidate_audio.clear();
                self.candidate_evidence.clear();
            }
            VoiceGateState::Idle => {}
        }

        self.reset_runtime_state();
        batches
    }

    pub fn state(&self) -> VoiceGateState {
        self.state
    }

    pub fn timeline_samples(&self) -> u64 {
        self.next_sample
    }

    fn validate_frame(
        &self,
        frame: &PcmFrame,
        probability: f32,
    ) -> Result<(), LocalVoiceGateInputError> {
        if frame.samples.is_empty() {
            return Err(LocalVoiceGateInputError::EmptyFrame);
        }
        if frame.valid_samples == 0 || frame.valid_samples > frame.samples.len() {
            return Err(LocalVoiceGateInputError::InvalidValidSampleCount);
        }
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(LocalVoiceGateInputError::InvalidProbability);
        }
        if frame.start_sample != self.next_sample {
            return Err(LocalVoiceGateInputError::DiscontinuousTimeline {
                expected: self.next_sample,
                actual: frame.start_sample,
            });
        }
        Ok(())
    }

    fn process_idle(&mut self, classified: ClassifiedFrame) {
        if classified.positive {
            self.candidate_audio = self.pre_roll.drain(..).collect();
            self.candidate_audio.push(classified.clone());
            self.candidate_evidence.push(classified);
            self.state = VoiceGateState::Candidate;
        } else {
            self.push_pre_roll(classified);
        }
    }

    fn process_candidate(&mut self, classified: ClassifiedFrame) {
        self.candidate_audio.push(classified.clone());
        self.candidate_evidence.push(classified);

        let evidence = measure_frames(&self.candidate_evidence);
        if evidence.positive_samples >= self.config.min_positive_samples
            && evidence.longest_positive_run_samples >= self.config.min_consecutive_positive_samples
        {
            self.active_audio = std::mem::take(&mut self.candidate_audio);
            self.candidate_evidence.clear();
            self.trailing_silence_samples = 0;
            self.state = VoiceGateState::Active;
        } else if evidence.real_samples >= self.config.activation_window_samples {
            let rejected = std::mem::take(&mut self.candidate_audio);
            self.candidate_evidence.clear();
            self.restore_pre_roll(rejected);
            self.state = VoiceGateState::Idle;
        }
    }

    fn process_active(&mut self, classified: ClassifiedFrame) -> Vec<ReadyBatch> {
        let valid_samples = classified.frame.valid_samples;
        let probability = classified.probability;
        self.active_audio.push(classified);

        if probability >= self.config.speech_threshold {
            self.trailing_silence_samples = 0;
            self.state = VoiceGateState::Active;
        } else if probability < self.config.negative_threshold {
            self.trailing_silence_samples =
                self.trailing_silence_samples.saturating_add(valid_samples);
            self.state = VoiceGateState::Hangover;
        } else if self.trailing_silence_samples > 0 {
            // The official iterator's neutral band neither starts nor cancels a
            // pending end; elapsed samples still advance its silence timer.
            self.trailing_silence_samples =
                self.trailing_silence_samples.saturating_add(valid_samples);
            self.state = VoiceGateState::Hangover;
        }

        if self.trailing_silence_samples >= self.config.end_silence_samples {
            return self.take_endpoint_batch();
        }
        self.take_max_batch()
    }

    fn take_max_batch(&mut self) -> Vec<ReadyBatch> {
        if self.active_audio.len() < self.config.max_batch_frames {
            return Vec::new();
        }

        let remainder = self.active_audio.split_off(self.config.max_batch_frames);
        let batch_frames = std::mem::replace(&mut self.active_audio, remainder);
        vec![make_batch(batch_frames, false)]
    }

    fn take_endpoint_batch(&mut self) -> Vec<ReadyBatch> {
        let trim_frames = frames_for_samples(
            self.trailing_silence_samples
                .saturating_sub(self.config.post_roll_frames * FRAME_SAMPLES),
        );
        let keep_frames = self.active_audio.len().saturating_sub(trim_frames);
        let discarded = self.active_audio.split_off(keep_frames);
        let batch_frames = std::mem::take(&mut self.active_audio);

        self.restore_pre_roll(discarded);
        self.candidate_audio.clear();
        self.candidate_evidence.clear();
        self.trailing_silence_samples = 0;
        self.state = VoiceGateState::Idle;

        if batch_frames.is_empty() {
            Vec::new()
        } else {
            vec![make_batch(batch_frames, true)]
        }
    }

    fn push_pre_roll(&mut self, classified: ClassifiedFrame) {
        if self.config.pre_roll_frames == 0 {
            return;
        }
        self.pre_roll.push_back(classified);
        while self.pre_roll.len() > self.config.pre_roll_frames {
            self.pre_roll.pop_front();
        }
    }

    fn restore_pre_roll(&mut self, frames: Vec<ClassifiedFrame>) {
        self.pre_roll.clear();
        if self.config.pre_roll_frames == 0 {
            return;
        }
        let start = frames.len().saturating_sub(self.config.pre_roll_frames);
        self.pre_roll.extend(frames.into_iter().skip(start));
    }

    fn reset_runtime_state(&mut self) {
        self.state = VoiceGateState::Idle;
        self.pre_roll.clear();
        self.candidate_audio.clear();
        self.candidate_evidence.clear();
        self.active_audio.clear();
        self.trailing_silence_samples = 0;
        self.next_sample = 0;
    }
}

#[derive(Debug, Default)]
struct FrameMeasurements {
    total_frames: usize,
    positive_frames: usize,
    probability_sum: f64,
    max_probability: f32,
    positive_samples: usize,
    longest_positive_run_frames: usize,
    longest_positive_run_samples: usize,
    longest_silence_run_frames: usize,
    longest_silence_run_samples: usize,
    real_samples: usize,
}

fn measure_frames(frames: &[ClassifiedFrame]) -> FrameMeasurements {
    let mut result = FrameMeasurements::default();
    let mut positive_run_frames = 0;
    let mut positive_run_samples = 0;
    let mut silence_run_frames = 0;
    let mut silence_run_samples = 0;

    for classified in frames {
        let valid_samples = classified.frame.valid_samples;
        result.total_frames += 1;
        result.real_samples += valid_samples;
        result.probability_sum += f64::from(classified.probability);
        result.max_probability = result.max_probability.max(classified.probability);

        if classified.positive {
            result.positive_frames += 1;
            result.positive_samples += valid_samples;
            positive_run_frames += 1;
            positive_run_samples += valid_samples;
            silence_run_frames = 0;
            silence_run_samples = 0;
            result.longest_positive_run_frames =
                result.longest_positive_run_frames.max(positive_run_frames);
            result.longest_positive_run_samples = result
                .longest_positive_run_samples
                .max(positive_run_samples);
        } else {
            silence_run_frames += 1;
            silence_run_samples += valid_samples;
            positive_run_frames = 0;
            positive_run_samples = 0;
            result.longest_silence_run_frames =
                result.longest_silence_run_frames.max(silence_run_frames);
            result.longest_silence_run_samples =
                result.longest_silence_run_samples.max(silence_run_samples);
        }
    }
    result
}

fn make_batch(frames: Vec<ClassifiedFrame>, ends_utterance: bool) -> ReadyBatch {
    let measurements = measure_frames(&frames);
    let mut samples = Vec::with_capacity(measurements.real_samples);
    for classified in &frames {
        samples.extend_from_slice(&classified.frame.samples[..classified.frame.valid_samples]);
    }

    let positive_ratio = if measurements.total_frames == 0 {
        0.0
    } else {
        measurements.positive_frames as f32 / measurements.total_frames as f32
    };
    let mean_probability = if measurements.total_frames == 0 {
        0.0
    } else {
        (measurements.probability_sum / measurements.total_frames as f64) as f32
    };

    ReadyBatch {
        samples,
        ends_utterance,
        metrics: VoiceBatchMetrics {
            total_frames: measurements.total_frames,
            positive_frames: measurements.positive_frames,
            positive_ratio,
            mean_probability,
            max_probability: measurements.max_probability,
            positive_samples: measurements.positive_samples,
            positive_duration_ms: duration_ms(measurements.positive_samples),
            longest_positive_run_frames: measurements.longest_positive_run_frames,
            longest_positive_run_ms: duration_ms(measurements.longest_positive_run_samples),
            longest_silence_run_frames: measurements.longest_silence_run_frames,
            longest_silence_run_ms: duration_ms(measurements.longest_silence_run_samples),
            real_samples: measurements.real_samples,
            total_duration_ms: duration_ms(measurements.real_samples),
        },
    }
}

fn frames_for_ms(duration_ms: u32) -> usize {
    (duration_ms / FRAME_DURATION_MS) as usize
}

fn samples_for_ms(duration_ms: u32) -> usize {
    frames_for_ms(duration_ms) * FRAME_SAMPLES
}

fn frames_for_samples(samples: usize) -> usize {
    samples / FRAME_SAMPLES
}

fn duration_ms(samples: usize) -> f64 {
    samples as f64 * 1_000.0 / f64::from(SAMPLE_RATE_HZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SILENCE: i16 = 0;
    const SPEECH: i16 = 1_000;
    const NEGATIVE: f32 = 0.05;
    const NEUTRAL: f32 = 0.42;
    const POSITIVE: f32 = 0.90;

    struct GateDriver {
        gate: LocalVoiceGate,
        next_sample: u64,
    }

    impl GateDriver {
        fn new() -> Self {
            Self {
                gate: LocalVoiceGate::new(LocalVoiceGateConfig::initial_test_hypothesis())
                    .expect("valid test hypothesis"),
                next_sample: 0,
            }
        }

        fn with_config(config: LocalVoiceGateConfig) -> Self {
            Self {
                gate: LocalVoiceGate::new(config).expect("valid test config"),
                next_sample: 0,
            }
        }

        fn push(&mut self, sample: i16, probability: f32) -> Vec<ReadyBatch> {
            let frame = PcmFrame {
                samples: vec![sample; FRAME_SAMPLES],
                valid_samples: FRAME_SAMPLES,
                start_sample: self.next_sample,
            };
            self.next_sample += FRAME_SAMPLES as u64;
            self.gate
                .push_evidence(frame, probability)
                .expect("valid deterministic evidence")
        }

        fn push_many(&mut self, sample: i16, probability: f32, count: usize) -> Vec<ReadyBatch> {
            let mut batches = Vec::new();
            for _ in 0..count {
                batches.extend(self.push(sample, probability));
            }
            batches
        }
    }

    #[test]
    fn negative_silero_evidence_keeps_idle() {
        let mut driver = GateDriver::new();
        assert!(driver.push_many(SILENCE, NEGATIVE, 100).is_empty());
        assert_eq!(driver.gate.state(), VoiceGateState::Idle);
        assert!(driver.gate.finish().is_empty());
    }

    #[test]
    fn isolated_false_positive_does_not_confirm_candidate() {
        let mut driver = GateDriver::new();
        driver.push(SPEECH, POSITIVE);
        assert_eq!(driver.gate.state(), VoiceGateState::Candidate);
        assert!(driver.push_many(SILENCE, NEGATIVE, 4).is_empty());
        assert_eq!(driver.gate.state(), VoiceGateState::Idle);
        assert!(driver.gate.finish().is_empty());
    }

    #[test]
    fn confirmed_short_speech_produces_ready_batch() {
        let mut driver = GateDriver::new();
        driver.push_many(SPEECH, POSITIVE, 2);
        let batches = driver.gate.finish();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].ends_utterance);
        assert_eq!(batches[0].samples.len(), 2 * FRAME_SAMPLES);
    }

    #[test]
    fn pre_roll_is_preserved() {
        let mut driver = GateDriver::new();
        driver.push_many(SILENCE, NEGATIVE, 10);
        driver.push_many(SPEECH, POSITIVE, 2);
        let batch = driver.gate.finish().pop().expect("confirmed batch");
        assert_eq!(batch.samples.len(), 12 * FRAME_SAMPLES);
        assert!(batch.samples[..10 * FRAME_SAMPLES]
            .iter()
            .all(|sample| *sample == SILENCE));
    }

    #[test]
    fn internal_pauses_are_preserved() {
        let mut driver = GateDriver::new();
        driver.push_many(SPEECH, POSITIVE, 2);
        driver.push_many(SILENCE, NEGATIVE, 2);
        driver.push(SPEECH, POSITIVE);
        let batch = driver.gate.finish().pop().expect("confirmed batch");
        let internal = &batch.samples[2 * FRAME_SAMPLES..4 * FRAME_SAMPLES];
        assert!(internal.iter().all(|sample| *sample == SILENCE));
    }

    #[test]
    fn resumption_during_hangover_cancels_endpoint() {
        let mut driver = GateDriver::new();
        driver.push_many(SPEECH, POSITIVE, 2);
        driver.push_many(SILENCE, NEGATIVE, 10);
        assert_eq!(driver.gate.state(), VoiceGateState::Hangover);
        driver.push(SPEECH, POSITIVE);
        assert_eq!(driver.gate.state(), VoiceGateState::Active);
        assert!(driver.push_many(SILENCE, NEGATIVE, 21).is_empty());
        assert_eq!(driver.push(SILENCE, NEGATIVE).len(), 1);
    }

    #[test]
    fn hysteresis_band_does_not_start_hangover_but_preserves_pending_end() {
        let mut driver = GateDriver::new();
        driver.push_many(SPEECH, POSITIVE, 2);
        driver.push_many(SPEECH, NEUTRAL, 5);
        assert_eq!(driver.gate.state(), VoiceGateState::Active);

        driver.push(SILENCE, NEGATIVE);
        assert_eq!(driver.gate.state(), VoiceGateState::Hangover);
        driver.push_many(SILENCE, NEUTRAL, 20);
        assert_eq!(driver.gate.state(), VoiceGateState::Hangover);
        assert_eq!(driver.push(SILENCE, NEUTRAL).len(), 1);
    }

    #[test]
    fn endpoint_sets_ends_utterance_and_keeps_exact_post_roll() {
        let mut driver = GateDriver::new();
        driver.push_many(SPEECH, POSITIVE, 2);
        let batches = driver.push_many(SILENCE, NEGATIVE, 22);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].ends_utterance);
        assert_eq!(batches[0].samples.len(), 8 * FRAME_SAMPLES);
        assert!(batches[0].samples[2 * FRAME_SAMPLES..]
            .iter()
            .all(|sample| *sample == SILENCE));
    }

    #[test]
    fn maximum_batch_does_not_end_utterance() {
        let mut config = LocalVoiceGateConfig::initial_test_hypothesis();
        config.max_batch_ms = 320;
        let mut driver = GateDriver::with_config(config);
        let batches = driver.push_many(SPEECH, POSITIVE, 10);
        assert_eq!(batches.len(), 1);
        assert!(!batches[0].ends_utterance);
        assert_eq!(batches[0].samples.len(), 10 * FRAME_SAMPLES);
        assert_eq!(driver.gate.state(), VoiceGateState::Active);
    }

    #[test]
    fn finish_sends_active_short_speech_and_discards_candidate() {
        let mut active = GateDriver::new();
        active.push_many(SPEECH, POSITIVE, 2);
        assert_eq!(active.gate.finish().len(), 1);

        let mut candidate = GateDriver::new();
        candidate.push(SPEECH, POSITIVE);
        assert!(candidate.gate.finish().is_empty());
        assert_eq!(candidate.gate.state(), VoiceGateState::Idle);
    }

    #[test]
    fn no_sample_is_lost_or_duplicated() {
        let mut driver = GateDriver::new();
        let pattern = [
            (11, POSITIVE),
            (22, POSITIVE),
            (33, NEGATIVE),
            (44, POSITIVE),
        ];
        for (sample, probability) in pattern {
            driver.push(sample, probability);
        }
        assert_eq!(
            driver.gate.timeline_samples(),
            (pattern.len() * FRAME_SAMPLES) as u64
        );
        let batch = driver.gate.finish().pop().expect("confirmed batch");
        let expected = pattern
            .into_iter()
            .flat_map(|(sample, _)| vec![sample; FRAME_SAMPLES])
            .collect::<Vec<_>>();
        assert_eq!(batch.samples, expected);
        assert_eq!(driver.gate.timeline_samples(), 0);
    }

    #[test]
    fn discontinuous_timeline_is_rejected_without_advancing() {
        let mut gate =
            LocalVoiceGate::new(LocalVoiceGateConfig::initial_test_hypothesis()).unwrap();
        let error = gate
            .push_evidence(
                PcmFrame {
                    samples: vec![0; FRAME_SAMPLES],
                    valid_samples: FRAME_SAMPLES,
                    start_sample: 10,
                },
                NEGATIVE,
            )
            .unwrap_err();
        assert_eq!(
            error,
            LocalVoiceGateInputError::DiscontinuousTimeline {
                expected: 0,
                actual: 10
            }
        );
        assert_eq!(gate.timeline_samples(), 0);
    }

    #[test]
    fn metrics_report_silero_probabilities_and_runs() {
        let mut driver = GateDriver::new();
        driver.push(SPEECH, 0.8);
        driver.push(SPEECH, 0.6);
        driver.push(SILENCE, 0.2);
        driver.push(SPEECH, 0.7);
        let batch = driver.gate.finish().pop().expect("confirmed batch");
        assert_eq!(batch.metrics.total_frames, 4);
        assert_eq!(batch.metrics.positive_frames, 3);
        assert!((batch.metrics.positive_ratio - 0.75).abs() < f32::EPSILON);
        assert!((batch.metrics.mean_probability - 0.575).abs() < 0.000_1);
        assert!((batch.metrics.max_probability - 0.8).abs() < f32::EPSILON);
        assert_eq!(batch.metrics.longest_positive_run_frames, 2);
        assert_eq!(batch.metrics.longest_silence_run_frames, 1);
        assert_eq!(batch.metrics.real_samples, 4 * FRAME_SAMPLES);
        assert_eq!(batch.metrics.total_duration_ms, 128.0);
    }

    #[test]
    fn gate_instances_do_not_share_state() {
        let mut first = GateDriver::new();
        let mut second = GateDriver::new();
        first.push_many(SPEECH, POSITIVE, 2);
        second.push(SPEECH, POSITIVE);
        assert_eq!(first.gate.state(), VoiceGateState::Active);
        assert_eq!(second.gate.state(), VoiceGateState::Candidate);
        assert_eq!(first.gate.finish().len(), 1);
        assert!(second.gate.finish().is_empty());
    }
}
