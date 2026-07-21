use std::sync::Arc;

use futures::future::BoxFuture;

use super::RegularTask;
use crate::session::session::Session;

impl Session {
    /// Starts a regular turn when the session is idle and pending work is waiting.
    ///
    /// Pending work currently includes mailbox mail marked with `trigger_turn`.
    ///
    /// This helper generates a fresh sub-id for the synthetic turn before delegating to the
    /// explicit-sub-id variant.
    pub(crate) fn maybe_start_turn_for_pending_work(self: &Arc<Self>) -> BoxFuture<'static, ()> {
        let session = Arc::clone(self);
        Box::pin(async move {
            session
                .maybe_start_turn_for_pending_work_with_sub_id(uuid::Uuid::new_v4().to_string())
                .await;
        })
    }

    /// Starts a regular turn with the provided sub-id when pending work should wake an idle
    /// session.
    ///
    /// The turn is created only when there is mailbox mail marked with `trigger_turn`, and only
    /// if the session is currently idle.
    pub(crate) async fn maybe_start_turn_for_pending_work_with_sub_id(
        self: &Arc<Self>,
        sub_id: String,
    ) {
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
            return;
        }

        {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.has_active_turn() {
                return;
            }
            let Some(_) = active_turn.reserve_taskless() else {
                unreachable!("idle turn slot must accept a taskless reservation");
            };
        }

        let turn_context = self.new_default_turn_with_sub_id(sub_id).await;
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        self.start_task(turn_context, Vec::new(), RegularTask::new())
            .await;
    }
}
