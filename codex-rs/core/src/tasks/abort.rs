use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;

use super::finalization::PendingFinalization;
use super::finalization::PendingFinalizationOutcome;
use crate::session::session::Session;
use crate::state::SessionTurnAbortTransition;
use crate::state::turn_lifecycle::TurnStartOutcome as LifecycleStartOutcome;

impl Session {
    fn invalidate_automatic_starts_for_abort(
        &self,
        reason: &TurnAbortReason,
    ) -> Option<crate::session::AutomaticTurnStartTicket> {
        match reason {
            TurnAbortReason::Interrupted => Some(
                self.turn_start_gate
                    .retry_automatic_starts_after_invalidation(),
            ),
            TurnAbortReason::Replaced
            | TurnAbortReason::ReviewEnded
            | TurnAbortReason::BudgetLimited => {
                self.turn_start_gate.suppress_automatic_starts();
                None
            }
        }
    }

    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        let (mut transition, retry_ticket) = {
            let mut active_turn = self.active_turn.lock().await;
            let transition = active_turn.begin_abort(reason.clone());
            let retry_ticket = if matches!(&transition, SessionTurnAbortTransition::Inactive) {
                self.turn_start_gate.suppress_automatic_starts();
                None
            } else {
                self.invalidate_automatic_starts_for_abort(&reason)
            };
            (transition, retry_ticket)
        };
        let finalizing_turn = loop {
            match transition {
                SessionTurnAbortTransition::Starting(generation) => {
                    match generation.wait_finished().await {
                        LifecycleStartOutcome::Committed => {
                            transition = self.active_turn.lock().await.begin_abort(reason.clone());
                        }
                        LifecycleStartOutcome::Cancelled(_) => {
                            if let Some(ticket) = retry_ticket {
                                self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                                    .await;
                            }
                            return;
                        }
                        LifecycleStartOutcome::Poisoned(_) => return,
                    }
                }
                SessionTurnAbortTransition::Running(finalizing_turn) => break finalizing_turn,
                SessionTurnAbortTransition::Finalizing(generation) => {
                    generation.wait_lifecycle_finished().await;
                    if let Some(ticket) = retry_ticket {
                        self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                            .await;
                    }
                    return;
                }
                SessionTurnAbortTransition::Inactive => {
                    if let Some(ticket) = retry_ticket {
                        self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                            .await;
                    }
                    return;
                }
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
            && let Some(ticket) = retry_ticket
        {
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
        }
    }

    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let (finalizing_turn, retry_ticket) = {
            let mut active = self.active_turn.lock().await;
            let Some(finalizing_turn) = active.begin_running_finalization_for_turn(turn_id) else {
                return false;
            };
            let retry_ticket = self.invalidate_automatic_starts_for_abort(&reason);
            (finalizing_turn, retry_ticket)
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
            && let Some(ticket) = retry_ticket
        {
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
        }

        true
    }
}

#[cfg(test)]
#[path = "abort_tests.rs"]
mod tests;
