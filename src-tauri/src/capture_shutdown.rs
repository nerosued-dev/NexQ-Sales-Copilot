use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CaptureTranscriptSegment {
    pub id: String,
    pub text: String,
    pub speaker: String,
    pub timestamp_ms: u64,
    pub is_final: bool,
    pub confidence: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CaptureShutdownResult {
    pub segments: Vec<CaptureTranscriptSegment>,
}

pub type ShutdownOutcome = Result<CaptureShutdownResult, String>;
pub type SharedTranscript = Arc<Mutex<Vec<CaptureTranscriptSegment>>>;

pub fn new_shared_transcript() -> SharedTranscript {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn upsert_segment(
    transcript: &SharedTranscript,
    segment: CaptureTranscriptSegment,
) -> Result<(), String> {
    let mut segments = transcript
        .lock()
        .map_err(|_| "Transcript shutdown state is unavailable".to_string())?;
    if let Some(existing) = segments.iter_mut().find(|item| item.id == segment.id) {
        *existing = segment;
    } else {
        segments.push(segment);
    }
    Ok(())
}

pub struct ActiveCaptureRuntime {
    pub pipeline_task: JoinHandle<Result<(), String>>,
    pub transcript_tasks: Vec<JoinHandle<Result<(), String>>>,
    pub transcript: SharedTranscript,
}

impl ActiveCaptureRuntime {
    pub async fn shutdown(self) -> ShutdownOutcome {
        let mut first_error = match self.pipeline_task.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("Capture pipeline terminated unexpectedly".to_string()),
        };

        for task in self.transcript_tasks {
            let task_error = match task.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("Transcript task terminated unexpectedly".to_string()),
            };
            if first_error.is_none() {
                first_error = task_error;
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }

        let segments = self
            .transcript
            .lock()
            .map_err(|_| "Transcript shutdown state is unavailable".to_string())?
            .clone();
        Ok(CaptureShutdownResult { segments })
    }
}

enum CaptureLifecycleState {
    Empty,
    Running(Option<ActiveCaptureRuntime>),
    Stopping(watch::Sender<Option<ShutdownOutcome>>),
    Complete(ShutdownOutcome),
}

pub enum StopDecision {
    Empty,
    Leader {
        runtime: ActiveCaptureRuntime,
        result_tx: watch::Sender<Option<ShutdownOutcome>>,
    },
    Follower(watch::Receiver<Option<ShutdownOutcome>>),
    Complete(ShutdownOutcome),
}

pub struct CaptureLifecycle {
    state: Mutex<CaptureLifecycleState>,
}

impl Default for CaptureLifecycle {
    fn default() -> Self {
        Self {
            state: Mutex::new(CaptureLifecycleState::Empty),
        }
    }
}

impl CaptureLifecycle {
    pub fn start(&self, runtime: ActiveCaptureRuntime) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Capture lifecycle state is unavailable".to_string())?;
        match &*state {
            CaptureLifecycleState::Empty | CaptureLifecycleState::Complete(_) => {
                *state = CaptureLifecycleState::Running(Some(runtime));
                Ok(())
            }
            CaptureLifecycleState::Running(_) | CaptureLifecycleState::Stopping(_) => {
                Err("Capture is already active or stopping".to_string())
            }
        }
    }

    pub fn begin_stop(&self) -> Result<StopDecision, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Capture lifecycle state is unavailable".to_string())?;
        match &mut *state {
            CaptureLifecycleState::Empty => Ok(StopDecision::Empty),
            CaptureLifecycleState::Running(runtime) => {
                let runtime = runtime
                    .take()
                    .ok_or_else(|| "Capture shutdown state is incomplete".to_string())?;
                let (result_tx, result_rx) = watch::channel(None);
                *state = CaptureLifecycleState::Stopping(result_tx.clone());
                drop(result_rx);
                Ok(StopDecision::Leader { runtime, result_tx })
            }
            CaptureLifecycleState::Stopping(result_tx) => {
                Ok(StopDecision::Follower(result_tx.subscribe()))
            }
            CaptureLifecycleState::Complete(outcome) => Ok(StopDecision::Complete(outcome.clone())),
        }
    }

    pub fn complete_stop(
        &self,
        result_tx: watch::Sender<Option<ShutdownOutcome>>,
        outcome: ShutdownOutcome,
    ) -> Result<(), String> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Capture lifecycle state is unavailable".to_string())?;
            *state = CaptureLifecycleState::Complete(outcome.clone());
        }
        result_tx.send_replace(Some(outcome));
        Ok(())
    }
}

pub async fn wait_for_stop_result(
    mut result_rx: watch::Receiver<Option<ShutdownOutcome>>,
) -> ShutdownOutcome {
    loop {
        if let Some(outcome) = result_rx.borrow().clone() {
            return outcome;
        }
        if result_rx.changed().await.is_err() {
            return Err("Capture shutdown result is unavailable".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    fn segment(id: &str, speaker: &str, is_final: bool) -> CaptureTranscriptSegment {
        CaptureTranscriptSegment {
            id: id.to_string(),
            text: format!("text-{id}"),
            speaker: speaker.to_string(),
            timestamp_ms: 10,
            is_final,
            confidence: 0.9,
            speaker_id: None,
        }
    }

    fn controlled_runtime(
        transcript: SharedTranscript,
    ) -> (
        ActiveCaptureRuntime,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    ) {
        let (request_started_tx, request_started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (result_tx, mut result_rx) = mpsc::channel(4);

        let pipeline_task = tokio::spawn(async move {
            request_started_tx
                .send(())
                .map_err(|_| "test start signal failed".to_string())?;
            release_rx
                .await
                .map_err(|_| "test release signal failed".to_string())?;
            result_tx
                .send(segment("you-1", "User", true))
                .await
                .map_err(|_| "test result send failed".to_string())?;
            Ok(())
        });

        let transcript_clone = transcript.clone();
        let transcript_task = tokio::spawn(async move {
            while let Some(item) = result_rx.recv().await {
                upsert_segment(&transcript_clone, item)?;
            }
            Ok(())
        });

        (
            ActiveCaptureRuntime {
                pipeline_task,
                transcript_tasks: vec![transcript_task],
                transcript,
            },
            request_started_rx,
            release_tx,
        )
    }

    #[tokio::test]
    async fn delayed_stt_result_keeps_shutdown_pending_until_finalization() {
        let transcript = new_shared_transcript();
        upsert_segment(&transcript, segment("you-1", "User", false)).unwrap();
        let (runtime, request_started, release) = controlled_runtime(transcript);
        request_started.await.unwrap();

        let mut shutdown = tokio::spawn(runtime.shutdown());
        assert!(matches!(
            futures::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));

        release.send(()).unwrap();
        let result = shutdown.await.unwrap().unwrap();
        assert_eq!(result.segments, vec![segment("you-1", "User", true)]);
    }

    #[tokio::test]
    async fn no_transcript_task_remains_after_shutdown_resolves() {
        let transcript = new_shared_transcript();
        let (runtime, request_started, release) = controlled_runtime(transcript.clone());
        request_started.await.unwrap();
        release.send(()).unwrap();

        let result = runtime.shutdown().await.unwrap();
        assert_eq!(result.segments.len(), 1);
        assert_eq!(transcript.lock().unwrap().len(), 1);
        assert_eq!(Arc::strong_count(&transcript), 1);
    }

    #[tokio::test]
    async fn concurrent_stop_calls_share_one_shutdown_result() {
        let lifecycle = Arc::new(CaptureLifecycle::default());
        let transcript = new_shared_transcript();
        let (runtime, request_started, release) = controlled_runtime(transcript);
        lifecycle.start(runtime).unwrap();
        request_started.await.unwrap();

        let (runtime, result_tx) = match lifecycle.begin_stop().unwrap() {
            StopDecision::Leader { runtime, result_tx } => (runtime, result_tx),
            _ => panic!("first stop must lead"),
        };
        let follower = match lifecycle.begin_stop().unwrap() {
            StopDecision::Follower(receiver) => receiver,
            _ => panic!("second stop must follow"),
        };

        let lifecycle_clone = lifecycle.clone();
        let leader = tokio::spawn(async move {
            let outcome = runtime.shutdown().await;
            lifecycle_clone
                .complete_stop(result_tx, outcome.clone())
                .unwrap();
            outcome
        });
        let follower = tokio::spawn(wait_for_stop_result(follower));

        release.send(()).unwrap();
        assert_eq!(leader.await.unwrap(), follower.await.unwrap());
    }

    #[tokio::test]
    async fn you_and_them_channels_finish_independently() {
        let transcript = new_shared_transcript();
        let (you_tx, you_rx) = oneshot::channel();
        let (them_tx, them_rx) = oneshot::channel();
        let (result_tx, mut result_rx) = mpsc::channel(4);

        let you_result_tx = result_tx.clone();
        let you_task = tokio::spawn(async move {
            you_rx.await.map_err(|_| "You failed".to_string())?;
            you_result_tx
                .send(segment("you", "User", true))
                .await
                .map_err(|_| "You result failed".to_string())?;
            Ok::<(), String>(())
        });
        let them_task = tokio::spawn(async move {
            them_rx.await.map_err(|_| "Them failed".to_string())?;
            result_tx
                .send(segment("them", "Them", true))
                .await
                .map_err(|_| "Them result failed".to_string())?;
            Ok::<(), String>(())
        });
        let transcript_clone = transcript.clone();
        let collector = tokio::spawn(async move {
            while let Some(item) = result_rx.recv().await {
                upsert_segment(&transcript_clone, item)?;
            }
            Ok(())
        });
        let pipeline_task = tokio::spawn(async move {
            let (you_result, them_result) = tokio::join!(you_task, them_task);
            you_result.map_err(|_| "You task failed".to_string())??;
            them_result.map_err(|_| "Them task failed".to_string())??;
            Ok(())
        });
        let runtime = ActiveCaptureRuntime {
            pipeline_task,
            transcript_tasks: vec![collector],
            transcript,
        };

        let mut shutdown = tokio::spawn(runtime.shutdown());
        you_tx.send(()).unwrap();
        assert!(matches!(
            futures::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        them_tx.send(()).unwrap();

        let result = shutdown.await.unwrap().unwrap();
        assert_eq!(result.segments.len(), 2);
    }
}
