use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;

use crate::session::session::Session;

impl Session {
    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        let mut aborted_turn = false;
        let mut turn_state_to_clear = None;
        let mut turn_context = None;
        if let Some(mut active_turn) = self.take_active_turn().await {
            let task = active_turn.take_running_task();
            aborted_turn = task.is_some();
            turn_context = task.as_ref().map(|task| Arc::clone(&task.turn_context));
            if let Some(task) = task {
                self.handle_task_abort(task, reason.clone()).await;
            }
            if aborted_turn {
                turn_state_to_clear = Some(active_turn.into_turn_state());
            }
        }

        if let Some(turn_context) = turn_context.as_deref() {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
        }
        if let Some(turn_state) = turn_state_to_clear {
            // Let interrupted tasks observe cancellation before dropping pending approvals, or an
            // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
            self.input_queue.clear_pending(turn_state.as_ref()).await;
        }
        if reason == TurnAbortReason::Interrupted && aborted_turn {
            self.maybe_start_turn_for_pending_work().await;
        }
    }

    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let active_turn = {
            let mut active = self.active_turn.lock().await;
            active.take_running_turn_for_abort(turn_id)
        };
        let Some(mut active_turn) = active_turn else {
            return false;
        };

        let task = active_turn.take_running_task();
        let turn_context = task.as_ref().map(|task| Arc::clone(&task.turn_context));
        if let Some(task) = task {
            self.handle_task_abort(task, reason.clone()).await;
        }
        if let Some(turn_context) = turn_context.as_deref() {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
        }
        // Let interrupted tasks observe cancellation before dropping pending approvals, or an
        // in-flight approval wait can surface as a model-visible rejection before TurnAborted.
        let turn_state = active_turn.into_turn_state();
        self.input_queue.clear_pending(turn_state.as_ref()).await;

        if reason == TurnAbortReason::Interrupted {
            self.maybe_start_turn_for_pending_work().await;
        }

        true
    }
}
