use serde::Deserialize;

pub(super) const HIGH_NO_SPEECH_PROBABILITY: f32 = 0.98;
pub(super) const LOW_NO_SPEECH_PROBABILITY: f32 = 0.80;
pub(super) const VERY_LOW_AVG_LOGPROB: f32 = -1.50;
pub(super) const LOW_AVG_LOGPROB: f32 = -0.50;

const HALLUCINATION_PHRASES: &[&str] = &[
    "thank you",
    "thanks",
    "thanks for watching",
    "thank you for watching",
    "you",
    "bye",
    "goodbye",
    "the end",
    "subtitle",
    "subtitles",
    "please subscribe",
    "like and subscribe",
    "thanks for listening",
    "thank you for listening",
];

#[derive(Debug, Deserialize)]
pub(super) struct GroqTranscriptionResponse {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub segments: Vec<GroqTranscriptionSegment>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GroqTranscriptionSegment {
    #[serde(default)]
    pub start: Option<f32>,
    #[serde(default)]
    pub end: Option<f32>,
    #[serde(default)]
    pub avg_logprob: Option<f32>,
    #[serde(default)]
    pub compression_ratio: Option<f32>,
    #[serde(default)]
    pub no_speech_prob: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NormalizedGroqTranscription {
    pub text: String,
    pub segment_count: usize,
    pub avg_logprob: Option<f32>,
    pub no_speech_prob: Option<f32>,
    pub compression_ratio: Option<f32>,
    pub has_timestamps: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RejectionReason {
    EmptyText,
    PunctuationOnly,
    KnownHallucination,
    HighNoSpeechProbability,
    VeryLowAverageLogProbability,
    CombinedNoSpeechSignals,
}

impl RejectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmptyText => "empty_text",
            Self::PunctuationOnly => "punctuation_only",
            Self::KnownHallucination => "known_hallucination",
            Self::HighNoSpeechProbability => "high_no_speech_probability",
            Self::VeryLowAverageLogProbability => "very_low_avg_logprob",
            Self::CombinedNoSpeechSignals => "combined_no_speech_signals",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TranscriptAcceptance {
    Speech { confidence: Option<f32> },
    Silence { reason: RejectionReason },
    Rejected { reason: RejectionReason },
}

impl TranscriptAcceptance {
    pub fn decision_name(&self) -> &'static str {
        match self {
            Self::Speech { .. } => "speech",
            Self::Silence { .. } => "silence",
            Self::Rejected { .. } => "rejected",
        }
    }

    pub fn reason(&self) -> Option<RejectionReason> {
        match self {
            Self::Speech { .. } => None,
            Self::Silence { reason } | Self::Rejected { reason } => Some(*reason),
        }
    }
}

pub(super) fn parse_groq_response(
    body: &[u8],
) -> Result<GroqTranscriptionResponse, &'static str> {
    serde_json::from_slice(body).map_err(|_| "Groq returned an invalid transcription response")
}

pub(super) fn normalize_response(
    response: GroqTranscriptionResponse,
) -> NormalizedGroqTranscription {
    let avg_logprob = finite_mean(response.segments.iter().filter_map(|segment| segment.avg_logprob));
    let no_speech_prob = finite_mean(
        response
            .segments
            .iter()
            .filter_map(|segment| segment.no_speech_prob),
    );
    let compression_ratio = finite_mean(
        response
            .segments
            .iter()
            .filter_map(|segment| segment.compression_ratio),
    );
    let has_timestamps = response.segments.iter().any(|segment| {
        segment.start.is_some_and(f32::is_finite) && segment.end.is_some_and(f32::is_finite)
    });

    NormalizedGroqTranscription {
        text: response.text.unwrap_or_default().trim().to_string(),
        segment_count: response.segments.len(),
        avg_logprob,
        no_speech_prob,
        compression_ratio,
        has_timestamps,
    }
}

fn finite_mean(values: impl Iterator<Item = f32>) -> Option<f32> {
    let (sum, count) = values
        .filter(|value| value.is_finite())
        .fold((0.0_f32, 0_u32), |(sum, count), value| {
            (sum + value, count + 1)
        });
    (count > 0).then_some(sum / count as f32)
}

fn normalized_comparison_text(text: &str) -> String {
    let lowercase = text.to_lowercase();
    let collapsed = lowercase.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|character: char| !character.is_alphanumeric())
        .to_string()
}

fn confidence_from_metadata(transcription: &NormalizedGroqTranscription) -> Option<f32> {
    match (transcription.avg_logprob, transcription.no_speech_prob) {
        (Some(avg_logprob), Some(no_speech_prob)) => {
            Some((avg_logprob.exp() * (1.0 - no_speech_prob.clamp(0.0, 1.0))).clamp(0.0, 1.0))
        }
        (Some(avg_logprob), None) => Some(avg_logprob.exp().clamp(0.0, 1.0)),
        (None, Some(no_speech_prob)) => Some((1.0 - no_speech_prob.clamp(0.0, 1.0)).clamp(0.0, 1.0)),
        (None, None) => None,
    }
}

pub(super) fn decide_transcript_acceptance(
    transcription: &NormalizedGroqTranscription,
) -> TranscriptAcceptance {
    if transcription.text.trim().is_empty() {
        return TranscriptAcceptance::Rejected {
            reason: RejectionReason::EmptyText,
        };
    }
    if !transcription.text.chars().any(char::is_alphanumeric) {
        return TranscriptAcceptance::Rejected {
            reason: RejectionReason::PunctuationOnly,
        };
    }
    if HALLUCINATION_PHRASES.contains(&normalized_comparison_text(&transcription.text).as_str()) {
        return TranscriptAcceptance::Rejected {
            reason: RejectionReason::KnownHallucination,
        };
    }

    match (transcription.no_speech_prob, transcription.avg_logprob) {
        (Some(no_speech_prob), _)
            if no_speech_prob >= HIGH_NO_SPEECH_PROBABILITY =>
        {
            TranscriptAcceptance::Silence {
                reason: RejectionReason::HighNoSpeechProbability,
            }
        }
        (_, Some(avg_logprob)) if avg_logprob <= VERY_LOW_AVG_LOGPROB => {
            TranscriptAcceptance::Rejected {
                reason: RejectionReason::VeryLowAverageLogProbability,
            }
        }
        (Some(no_speech_prob), Some(avg_logprob))
            if no_speech_prob >= LOW_NO_SPEECH_PROBABILITY
                && avg_logprob <= LOW_AVG_LOGPROB =>
        {
            TranscriptAcceptance::Silence {
                reason: RejectionReason::CombinedNoSpeechSignals,
            }
        }
        _ => TranscriptAcceptance::Speech {
            confidence: confidence_from_metadata(transcription),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcription(
        text: &str,
        avg_logprob: Option<f32>,
        no_speech_prob: Option<f32>,
    ) -> NormalizedGroqTranscription {
        NormalizedGroqTranscription {
            text: text.to_string(),
            segment_count: usize::from(avg_logprob.is_some() || no_speech_prob.is_some()),
            avg_logprob,
            no_speech_prob,
            compression_ratio: None,
            has_timestamps: false,
        }
    }

    #[test]
    fn rejects_empty_response() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("", None, None)),
            TranscriptAcceptance::Rejected {
                reason: RejectionReason::EmptyText
            }
        );
    }

    #[test]
    fn rejects_period_and_other_punctuation_only() {
        for text in [".", "?!", "… — !!!"] {
            assert_eq!(
                decide_transcript_acceptance(&transcription(text, None, None)),
                TranscriptAcceptance::Rejected {
                    reason: RejectionReason::PunctuationOnly
                }
            );
        }
    }

    #[test]
    fn rejects_whitespace_only() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("  \n\t ", None, None)),
            TranscriptAcceptance::Rejected {
                reason: RejectionReason::EmptyText
            }
        );
    }

    #[test]
    fn normalizes_hallucination_case_spacing_and_boundary_punctuation() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("  THANK   YOU...  ", None, None)),
            TranscriptAcceptance::Rejected {
                reason: RejectionReason::KnownHallucination
            }
        );
    }

    #[test]
    fn rejects_extreme_no_speech_probability() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("texto inventado", Some(-0.1), Some(0.99))),
            TranscriptAcceptance::Silence {
                reason: RejectionReason::HighNoSpeechProbability
            }
        );
    }

    #[test]
    fn rejects_very_low_average_log_probability() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("texto incerto", Some(-1.6), Some(0.1))),
            TranscriptAcceptance::Rejected {
                reason: RejectionReason::VeryLowAverageLogProbability
            }
        );
    }

    #[test]
    fn contradictory_signals_preserve_possible_speech() {
        assert!(matches!(
            decide_transcript_acceptance(&transcription("sim", Some(-0.05), Some(0.90))),
            TranscriptAcceptance::Speech { .. }
        ));
    }

    #[test]
    fn accepts_response_without_optional_metadata_with_unknown_confidence() {
        assert_eq!(
            decide_transcript_acceptance(&transcription("Bom dia", None, None)),
            TranscriptAcceptance::Speech { confidence: None }
        );
    }

    #[test]
    fn accepts_short_legitimate_speech() {
        for text in ["sim", "não", "certo", "Ana"] {
            assert!(matches!(
                decide_transcript_acceptance(&transcription(text, Some(-0.1), Some(0.02))),
                TranscriptAcceptance::Speech { .. }
            ));
        }
    }

    #[test]
    fn accepts_normal_portuguese_speech() {
        assert!(matches!(
            decide_transcript_acceptance(&transcription(
                "Podemos marcar a próxima conversa para amanhã?",
                Some(-0.2),
                Some(0.03)
            )),
            TranscriptAcceptance::Speech { .. }
        ));
    }

    #[test]
    fn confidence_is_derived_instead_of_constant() {
        let high = decide_transcript_acceptance(&transcription("sim", Some(-0.05), Some(0.01)));
        let lower = decide_transcript_acceptance(&transcription("sim", Some(-0.8), Some(0.2)));
        assert_ne!(high, lower);
        assert!(!matches!(high, TranscriptAcceptance::Speech { confidence: Some(value) } if value == 0.95));
    }

    #[test]
    fn parse_error_is_sanitized() {
        let error = parse_groq_response(br#"{"secret":"not valid"#).unwrap_err();
        assert_eq!(error, "Groq returned an invalid transcription response");
        assert!(!error.contains("secret"));
    }

    #[test]
    fn normalizes_verbose_segment_metadata() {
        let body = r#"{
                "text":" Olá ",
                "segments":[
                    {"start":0.0,"end":1.0,"avg_logprob":-0.1,"compression_ratio":1.2,"no_speech_prob":0.02},
                    {"start":1.0,"end":2.0,"avg_logprob":-0.3,"compression_ratio":1.4,"no_speech_prob":0.04}
                ]
            }"#;
        let parsed = parse_groq_response(body.as_bytes()).unwrap();
        let normalized = normalize_response(parsed);
        assert_eq!(normalized.text, "Olá");
        assert_eq!(normalized.segment_count, 2);
        assert!((normalized.avg_logprob.unwrap() + 0.2).abs() < f32::EPSILON);
        assert!((normalized.no_speech_prob.unwrap() - 0.03).abs() < f32::EPSILON);
        assert!(normalized.has_timestamps);
    }
}
