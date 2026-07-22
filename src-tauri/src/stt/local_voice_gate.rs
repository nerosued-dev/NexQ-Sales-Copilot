//! Pure, deterministic local voice gate for 16 kHz mono PCM.
//!
//! This module is deliberately independent from capture callbacks, STT providers,
//! networking, Tauri, persistence, and wall-clock time. It is not connected to
//! the running transcription pipeline yet.

use std::collections::VecDeque;
use std::fmt;

pub const SAMPLE_RATE_HZ: u32 = 16_000;
pub const FRAME_DURATION_MS: u32 = 20;
pub const FRAME_SAMPLES: usize = 320;

/// A frame emitted by [`FixedFrameSplitter`].
///
/// Full frames contain [`FRAME_SAMPLES`] samples. The explicit final frame may
/// contain fewer samples; `valid_samples` always describes the real PCM and no
/// zero padding is included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmFrame {
    pub samples: Vec<i16>,
    pub valid_samples: usize,
    pub start_sample: u64,
}

impl PcmFrame {
    pub fn rms(&self) -> f32 {
        let valid_samples = self.valid_samples.min(self.samples.len());
        instantaneous_rms(&self.samples[..valid_samples])
    }
}

/// Splits variable-sized PCM chunks into fixed-size frames without loss.
#[derive(Debug)]
pub struct FixedFrameSplitter {
    frame_samples: usize,
    remainder: Vec<i16>,
    emitted_samples: u64,
    accepted_samples: u64,
}

impl FixedFrameSplitter {
    pub fn new(frame_samples: usize) -> Result<Self, LocalVoiceGateConfigError> {
        if frame_samples == 0 {
            return Err(LocalVoiceGateConfigError::new(
                "frame_samples must be greater than zero",
            ));
        }

        Ok(Self {
            frame_samples,
            remainder: Vec::new(),
            emitted_samples: 0,
            accepted_samples: 0,
        })
    }

    /// Accepts any number of samples and returns only complete frames.
    pub fn push_samples(&mut self, samples: &[i16]) -> Vec<PcmFrame> {
        self.accepted_samples = self.accepted_samples.saturating_add(samples.len() as u64);
        self.remainder.extend_from_slice(samples);

        let complete_samples = self.remainder.len() / self.frame_samples * self.frame_samples;
        if complete_samples == 0 {
            return Vec::new();
        }

        let pending = std::mem::take(&mut self.remainder);
        let mut frames = Vec::with_capacity(complete_samples / self.frame_samples);
        for samples in pending[..complete_samples].chunks_exact(self.frame_samples) {
            frames.push(self.make_frame(samples.to_vec()));
        }
        self.remainder
            .extend_from_slice(&pending[complete_samples..]);
        frames
    }

    /// Emits the real, potentially partial remainder without zero padding.
    pub fn finish(&mut self) -> Option<PcmFrame> {
        if self.remainder.is_empty() {
            return None;
        }
        let samples = std::mem::take(&mut self.remainder);
        Some(self.make_frame(samples))
    }

    pub fn remainder_len(&self) -> usize {
        self.remainder.len()
    }

    /// Total real samples accepted, including the current remainder.
    pub fn timeline_samples(&self) -> u64 {
        self.accepted_samples
    }

    fn make_frame(&mut self, samples: Vec<i16>) -> PcmFrame {
        let valid_samples = samples.len();
        let frame = PcmFrame {
            samples,
            valid_samples,
            start_sample: self.emitted_samples,
        };
        self.emitted_samples = self.emitted_samples.saturating_add(valid_samples as u64);
        frame
    }
}

/// Calculates RMS over real i16 PCM samples without smoothing or state.
pub fn instantaneous_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_squares = samples
        .iter()
        .map(|&sample| {
            let sample = f64::from(sample);
            sample * sample
        })
        .sum::<f64>();
    (sum_squares / samples.len() as f64).sqrt() as f32
}

/// Configurable hypotheses for the first deterministic gate experiments.
///
/// Durations must be aligned to the 20 ms frame size. The constructor validates
/// all relationships so changing parameters does not require changing gate logic.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalVoiceGateConfig {
    pub rms_threshold: f32,
    pub pre_roll_ms: u32,
    pub activation_window_ms: u32,
    pub min_positive_ms: u32,
    pub min_consecutive_positive_frames: usize,
    pub end_silence_ms: u32,
    pub post_roll_ms: u32,
    pub max_batch_ms: u32,
}

impl LocalVoiceGateConfig {
    /// Initial values for deterministic tests, not calibrated production defaults.
    pub fn initial_test_hypothesis() -> Self {
        Self {
            rms_threshold: 300.0,
            pre_roll_ms: 300,
            activation_window_ms: 200,
            min_positive_ms: 60,
            min_consecutive_positive_frames: 2,
            end_silence_ms: 700,
            post_roll_ms: 200,
            max_batch_ms: 5_000,
        }
    }

    fn validate(&self) -> Result<DerivedConfig, LocalVoiceGateConfigError> {
        if !self.rms_threshold.is_finite() || self.rms_threshold < 0.0 {
            return Err(LocalVoiceGateConfigError::new(
                "rms_threshold must be finite and non-negative",
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
                    "pre_roll_ms" => "pre_roll_ms must align to 20 ms frames",
                    "activation_window_ms" => "activation_window_ms must align to 20 ms frames",
                    "min_positive_ms" => "min_positive_ms must align to 20 ms frames",
                    "end_silence_ms" => "end_silence_ms must align to 20 ms frames",
                    "post_roll_ms" => "post_roll_ms must align to 20 ms frames",
                    _ => "max_batch_ms must align to 20 ms frames",
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
            rms_threshold: self.rms_threshold,
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

#[derive(Debug, Clone, Copy)]
struct DerivedConfig {
    rms_threshold: f32,
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
    positive: bool,
}

/// Deterministic, per-instance voice gate.
#[derive(Debug)]
pub struct LocalVoiceGate {
    splitter: FixedFrameSplitter,
    config: DerivedConfig,
    state: VoiceGateState,
    pre_roll: VecDeque<ClassifiedFrame>,
    candidate_audio: Vec<ClassifiedFrame>,
    candidate_evidence: Vec<ClassifiedFrame>,
    active_audio: Vec<ClassifiedFrame>,
    trailing_silence_samples: usize,
}

impl LocalVoiceGate {
    pub fn new(config: LocalVoiceGateConfig) -> Result<Self, LocalVoiceGateConfigError> {
        let config = config.validate()?;
        Ok(Self {
            splitter: FixedFrameSplitter::new(FRAME_SAMPLES)?,
            config,
            state: VoiceGateState::Idle,
            pre_roll: VecDeque::new(),
            candidate_audio: Vec::new(),
            candidate_evidence: Vec::new(),
            active_audio: Vec::new(),
            trailing_silence_samples: 0,
        })
    }

    /// Processes variable-sized 16 kHz mono PCM chunks.
    pub fn push_samples(&mut self, samples: &[i16]) -> Vec<ReadyBatch> {
        let frames = self.splitter.push_samples(samples);
        let mut batches = Vec::new();
        for frame in frames {
            batches.extend(self.process_frame(frame));
        }
        batches
    }

    /// Explicitly classifies the real remainder, then finalizes confirmed speech.
    pub fn finish(&mut self) -> Vec<ReadyBatch> {
        let mut batches = Vec::new();
        if let Some(frame) = self.splitter.finish() {
            batches.extend(self.process_frame(frame));
        }

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

    pub fn remainder_len(&self) -> usize {
        self.splitter.remainder_len()
    }

    pub fn timeline_samples(&self) -> u64 {
        self.splitter.timeline_samples()
    }

    fn process_frame(&mut self, frame: PcmFrame) -> Vec<ReadyBatch> {
        let positive = frame.rms() >= self.config.rms_threshold;
        let classified = ClassifiedFrame { frame, positive };

        match self.state {
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
        }
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
        let positive = classified.positive;
        self.active_audio.push(classified);

        if positive {
            self.trailing_silence_samples = 0;
            self.state = VoiceGateState::Active;
        } else {
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
    }
}

#[derive(Debug, Default)]
struct FrameMeasurements {
    total_frames: usize,
    positive_frames: usize,
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

    ReadyBatch {
        samples,
        ends_utterance,
        metrics: VoiceBatchMetrics {
            total_frames: measurements.total_frames,
            positive_frames: measurements.positive_frames,
            positive_ratio,
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
    const QUIET_NOISE: i16 = 299;
    const SPEECH: i16 = 1_000;

    fn splitter() -> FixedFrameSplitter {
        FixedFrameSplitter::new(FRAME_SAMPLES).expect("valid fixed frame size")
    }

    fn gate() -> LocalVoiceGate {
        LocalVoiceGate::new(LocalVoiceGateConfig::initial_test_hypothesis())
            .expect("valid test hypothesis")
    }

    fn samples(value: i16, frames: usize) -> Vec<i16> {
        vec![value; frames * FRAME_SAMPLES]
    }

    fn push_frame(gate: &mut LocalVoiceGate, value: i16) -> Vec<ReadyBatch> {
        gate.push_samples(&samples(value, 1))
    }

    fn push_frames(gate: &mut LocalVoiceGate, value: i16, count: usize) -> Vec<ReadyBatch> {
        gate.push_samples(&samples(value, count))
    }

    fn collect_frames(mut splitter: FixedFrameSplitter, chunks: &[&[i16]]) -> Vec<PcmFrame> {
        let mut frames = Vec::new();
        for chunk in chunks {
            frames.extend(splitter.push_samples(chunk));
        }
        if let Some(frame) = splitter.finish() {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn one_sample_becomes_explicit_remainder() {
        let mut splitter = splitter();
        assert!(splitter.push_samples(&[7]).is_empty());
        assert_eq!(splitter.remainder_len(), 1);
        let frame = splitter.finish().expect("one-sample remainder");
        assert_eq!(frame.samples, vec![7]);
        assert_eq!(frame.valid_samples, 1);
        assert_eq!(frame.start_sample, 0);
    }

    #[test]
    fn three_hundred_nineteen_samples_remain_pending() {
        let mut splitter = splitter();
        assert!(splitter.push_samples(&vec![8; 319]).is_empty());
        assert_eq!(splitter.remainder_len(), 319);
        assert_eq!(splitter.finish().expect("partial frame").valid_samples, 319);
    }

    #[test]
    fn three_hundred_twenty_samples_form_one_frame() {
        let mut splitter = splitter();
        let frames = splitter.push_samples(&vec![9; 320]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].valid_samples, 320);
        assert_eq!(frames[0].start_sample, 0);
        assert_eq!(splitter.remainder_len(), 0);
    }

    #[test]
    fn three_hundred_twenty_one_samples_leave_one_sample() {
        let mut splitter = splitter();
        let frames = splitter.push_samples(&vec![10; 321]);
        assert_eq!(frames.len(), 1);
        assert_eq!(splitter.remainder_len(), 1);
        let remainder = splitter.finish().expect("one remaining sample");
        assert_eq!(remainder.valid_samples, 1);
        assert_eq!(remainder.start_sample, 320);
    }

    #[test]
    fn multiple_chunks_form_exactly_one_frame() {
        let first = vec![1; 100];
        let second = vec![2; 100];
        let third = vec![3; 120];
        let frames = collect_frames(splitter(), &[&first, &second, &third]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].valid_samples, FRAME_SAMPLES);
        assert_eq!(&frames[0].samples[..100], first.as_slice());
        assert_eq!(&frames[0].samples[100..200], second.as_slice());
        assert_eq!(&frames[0].samples[200..], third.as_slice());
    }

    #[test]
    fn one_chunk_produces_multiple_frames() {
        let mut splitter = splitter();
        let frames = splitter.push_samples(&vec![4; FRAME_SAMPLES * 3]);
        assert_eq!(frames.len(), 3);
        assert!(frames
            .iter()
            .all(|frame| frame.valid_samples == FRAME_SAMPLES));
        assert_eq!(frames[2].start_sample, (FRAME_SAMPLES * 2) as u64);
    }

    #[test]
    fn splitter_loses_no_samples() {
        let input: Vec<i16> = (0..1_003).map(|value| value as i16).collect();
        let chunks = [&input[..1], &input[1..320], &input[320..777], &input[777..]];
        let frames = collect_frames(splitter(), &chunks);
        let output_len: usize = frames.iter().map(|frame| frame.valid_samples).sum();
        assert_eq!(output_len, input.len());
    }

    #[test]
    fn splitter_duplicates_no_samples() {
        let input: Vec<i16> = (0..1_003).map(|value| value as i16).collect();
        let chunks = [
            &input[..17],
            &input[17..500],
            &input[500..999],
            &input[999..],
        ];
        let frames = collect_frames(splitter(), &chunks);
        let output: Vec<i16> = frames.into_iter().flat_map(|frame| frame.samples).collect();
        assert_eq!(output, input);
    }

    #[test]
    fn remainder_rms_uses_only_valid_samples() {
        let mut splitter = splitter();
        splitter.push_samples(&[1_000]);
        let frame = splitter.finish().expect("one real sample");
        assert_eq!(frame.valid_samples, 1);
        assert_eq!(frame.samples.len(), 1);
        assert!((frame.rms() - 1_000.0).abs() < f32::EPSILON);
    }

    #[test]
    fn absolute_silence_produces_no_batch() {
        let mut gate = gate();
        assert!(push_frames(&mut gate, SILENCE, 100).is_empty());
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn noise_below_threshold_produces_no_batch() {
        let mut gate = gate();
        assert!(push_frames(&mut gate, QUIET_NOISE, 100).is_empty());
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn one_positive_impulse_does_not_activate() {
        let mut gate = gate();
        assert!(push_frame(&mut gate, SPEECH).is_empty());
        assert!(push_frames(&mut gate, SILENCE, 9).is_empty());
        assert_eq!(gate.state(), VoiceGateState::Idle);
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn two_separated_impulses_do_not_activate() {
        let mut gate = gate();
        push_frame(&mut gate, SPEECH);
        push_frame(&mut gate, SILENCE);
        push_frame(&mut gate, SPEECH);
        assert!(push_frames(&mut gate, SILENCE, 7).is_empty());
        assert_eq!(gate.state(), VoiceGateState::Idle);
        assert!(gate.finish().is_empty());
    }

    #[test]
    fn rejected_candidate_returns_to_idle() {
        let mut gate = gate();
        push_frame(&mut gate, SPEECH);
        assert_eq!(gate.state(), VoiceGateState::Candidate);
        push_frames(&mut gate, SILENCE, 9);
        assert_eq!(gate.state(), VoiceGateState::Idle);
    }

    #[test]
    fn confirmed_short_speech_includes_pre_roll() {
        let mut gate = gate();
        push_frames(&mut gate, SILENCE, 15);
        push_frames(&mut gate, SPEECH, 3);
        let batches = push_frames(&mut gate, SILENCE, 35);
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert!(batch.ends_utterance);
        assert_eq!(batch.samples.len(), (15 + 3 + 10) * FRAME_SAMPLES);
        assert!(batch.samples[..15 * FRAME_SAMPLES]
            .iter()
            .all(|sample| *sample == SILENCE));
    }

    #[test]
    fn internal_negative_frames_remain_in_active_audio() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        push_frames(&mut gate, SILENCE, 2);
        push_frame(&mut gate, SPEECH);
        let batches = gate.finish();
        assert_eq!(batches.len(), 1);
        let internal = &batches[0].samples[3 * FRAME_SAMPLES..5 * FRAME_SAMPLES];
        assert!(internal.iter().all(|sample| *sample == SILENCE));
    }

    #[test]
    fn speech_resumption_during_hangover_cancels_endpoint() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        push_frames(&mut gate, SILENCE, 20);
        assert_eq!(gate.state(), VoiceGateState::Hangover);
        push_frame(&mut gate, SPEECH);
        assert_eq!(gate.state(), VoiceGateState::Active);
        assert!(push_frames(&mut gate, SILENCE, 34).is_empty());
        assert_eq!(gate.state(), VoiceGateState::Hangover);
        assert_eq!(push_frame(&mut gate, SILENCE).len(), 1);
        assert_eq!(gate.state(), VoiceGateState::Idle);
    }

    #[test]
    fn seven_hundred_ms_of_silence_produces_endpoint() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        assert!(push_frames(&mut gate, SILENCE, 34).is_empty());
        let batches = push_frame(&mut gate, SILENCE);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].ends_utterance);
        assert_eq!(gate.state(), VoiceGateState::Idle);
    }

    #[test]
    fn endpoint_preserves_exact_post_roll() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        let batches = push_frames(&mut gate, SILENCE, 35);
        let batch = &batches[0];
        assert_eq!(batch.samples.len(), 13 * FRAME_SAMPLES);
        let post_roll = &batch.samples[3 * FRAME_SAMPLES..];
        assert_eq!(post_roll.len(), 10 * FRAME_SAMPLES);
        assert!(post_roll.iter().all(|sample| *sample == SILENCE));
    }

    #[test]
    fn continuous_speech_produces_non_ending_max_batch() {
        let mut gate = gate();
        let batches = push_frames(&mut gate, SPEECH, 250);
        assert_eq!(batches.len(), 1);
        assert!(!batches[0].ends_utterance);
        assert_eq!(batches[0].samples.len(), 5 * SAMPLE_RATE_HZ as usize);
        assert_eq!(gate.state(), VoiceGateState::Active);
    }

    #[test]
    fn finish_emits_confirmed_residual_shorter_than_half_second() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        let batches = gate.finish();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].ends_utterance);
        assert_eq!(
            batches[0].samples.len(),
            60 * SAMPLE_RATE_HZ as usize / 1_000
        );
    }

    #[test]
    fn finish_discards_unconfirmed_candidate() {
        let mut gate = gate();
        push_frame(&mut gate, SPEECH);
        assert_eq!(gate.state(), VoiceGateState::Candidate);
        assert!(gate.finish().is_empty());
        assert_eq!(gate.state(), VoiceGateState::Idle);
    }

    #[test]
    fn known_temporal_pattern_has_exact_metrics() {
        let mut gate = gate();
        push_frames(&mut gate, SPEECH, 3);
        push_frames(&mut gate, SILENCE, 2);
        push_frame(&mut gate, SPEECH);
        push_frames(&mut gate, SILENCE, 3);
        let batches = gate.finish();
        let metrics = &batches[0].metrics;
        assert_eq!(metrics.total_frames, 9);
        assert_eq!(metrics.positive_frames, 4);
        assert!((metrics.positive_ratio - 4.0 / 9.0).abs() < f32::EPSILON);
        assert_eq!(metrics.positive_samples, 4 * FRAME_SAMPLES);
        assert_eq!(metrics.positive_duration_ms, 80.0);
        assert_eq!(metrics.longest_positive_run_frames, 3);
        assert_eq!(metrics.longest_positive_run_ms, 60.0);
        assert_eq!(metrics.longest_silence_run_frames, 3);
        assert_eq!(metrics.longest_silence_run_ms, 60.0);
        assert_eq!(metrics.real_samples, 9 * FRAME_SAMPLES);
        assert_eq!(metrics.total_duration_ms, 180.0);
    }

    #[test]
    fn gate_instances_do_not_share_state() {
        let mut first = gate();
        let mut second = gate();
        push_frames(&mut first, SPEECH, 3);
        push_frame(&mut second, SPEECH);
        assert_eq!(first.state(), VoiceGateState::Active);
        assert_eq!(second.state(), VoiceGateState::Candidate);
        assert_eq!(first.finish().len(), 1);
        assert!(second.finish().is_empty());
    }
}
