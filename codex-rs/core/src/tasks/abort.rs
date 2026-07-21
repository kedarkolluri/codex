use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;

use super::finalization::PendingFinalization;
use super::finalization::PendingFinalizationOutcome;
use crate::session::session::Session;
use crate::state::SessionTurnAbortTransition;
use crate::state::turn_lifecycle::TurnStartOutcome;

impl Session {
    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        let mut transition = self.active_turn.lock().await.begin_abort(reason.clone());
        let finalizing_turn = loop {
            match transition {
                SessionTurnAbortTransition::Starting(generation) => {
                    match generation.wait_finished().await {
                        TurnStartOutcome::Committed => {
                            transition = self.active_turn.lock().await.begin_abort(reason.clone());
                        }
                        TurnStartOutcome::Cancelled(_) | TurnStartOutcome::Poisoned(_) => return,
                    }
                }
                SessionTurnAbortTransition::Running(finalizing_turn) => break finalizing_turn,
                SessionTurnAbortTransition::Finalizing(generation) => {
                    generation.wait_lifecycle_finished().await;
                    return;
                }
                SessionTurnAbortTransition::Inactive => return,
            }
        };
        let (task, completion) = finalizing_turn.into_parts();
        let turn_context = Arc::clone(&task.turn_context);
        let pending_finalization = PendingFinalization::new(Arc::clone(self), completion);
        let turn_state = Arc::clone(pending_finalization.turn_state());
        self.handle_task_abort(task, reason.clone()).await;
        self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
            .await;
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        self.input_queue.clear_pending(turn_state.as_ref()).await;
        let completion = pending_finalization.complete().await;
        if completion == PendingFinalizationOutcome::Completed
            && reason == TurnAbortReason::Interrupted
        {
            self.maybe_start_turn_for_pending_work().await;
        }
    }

    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let finalizing_turn = {
            let mut active = self.active_turn.lock().await;
            active.begin_running_finalization_for_turn(turn_id)
        };
        let Some(finalizing_turn) = finalizing_turn else {
            return false;
        };
        let (task, completion) = finalizing_turn.into_parts();
        let turn_context = Arc::clone(&task.turn_context);
        let pending_finalization = PendingFinalization::new(Arc::clone(self), completion);
        let turn_state = Arc::clone(pending_finalization.turn_state());
        self.handle_task_abort(task, reason.clone()).await;
        self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
            .await;
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        self.input_queue.clear_pending(turn_state.as_ref()).await;
        let completion = pending_finalization.complete().await;

        if completion == PendingFinalizationOutcome::Completed
            && reason == TurnAbortReason::Interrupted
        {
            self.maybe_start_turn_for_pending_work().await;
        }

        true
    }
}
