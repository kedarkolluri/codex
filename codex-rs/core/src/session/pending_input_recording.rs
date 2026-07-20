use super::input_queue::PendingInputClaim;
use super::input_queue::PendingInputClaimMode;
use super::input_queue::PendingInputRecordingResult;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::state::TurnState;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInputRecordResult {
    Inactive,
    Empty,
    Recorded { should_stop: bool },
    Failed,
}

impl Session {
    pub(crate) async fn record_pending_input_for_turn(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        mode: PendingInputClaimMode,
    ) -> PendingInputRecordResult {
        let recording = loop {
            let claim = self
                .input_queue
                .claim_pending_input_for_turn(&self.active_turn, &turn_context.sub_id, mode)
                .await;
            match claim {
                PendingInputClaim::Inactive => return PendingInputRecordResult::Inactive,
                PendingInputClaim::Empty => return PendingInputRecordResult::Empty,
                PendingInputClaim::Recording(recording) => match recording.wait().await {
                    PendingInputRecordingResult::Completed { should_stop: true } => {
                        return PendingInputRecordResult::Recorded { should_stop: true };
                    }
                    PendingInputRecordingResult::Completed { should_stop: false } => continue,
                    PendingInputRecordingResult::Failed => {
                        warn!(
                            turn_id = %turn_context.sub_id,
                            "pending-input recorder stopped before reporting completion"
                        );
                        return PendingInputRecordResult::Failed;
                    }
                },
                PendingInputClaim::Acquired(claim) => {
                    let recording = claim.recording();
                    let (items, completion) = claim.into_parts();
                    let sess = Arc::clone(self);
                    let turn_context = Arc::clone(turn_context);
                    tokio::spawn(async move {
                        let should_stop = super::turn::run_hooks_and_record_inputs(
                            &sess,
                            &turn_context,
                            &items,
                        )
                        .await;
                        completion
                            .finish(PendingInputRecordingResult::Completed { should_stop })
                            .await;
                    });
                    break recording;
                }
            }
        };

        match recording.wait().await {
            PendingInputRecordingResult::Completed { should_stop } => {
                PendingInputRecordResult::Recorded { should_stop }
            }
            PendingInputRecordingResult::Failed => {
                warn!(
                    turn_id = %turn_context.sub_id,
                    "pending-input recorder stopped before reporting completion"
                );
                PendingInputRecordResult::Failed
            }
        }
    }

    pub(crate) async fn wait_for_pending_input_recording(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        let Some(recording) = self
            .input_queue
            .pending_input_recording(turn_state)
            .await
        else {
            return;
        };
        if recording.wait().await == PendingInputRecordingResult::Failed {
            warn!("pending-input recorder stopped during turn teardown");
        }
    }
}
