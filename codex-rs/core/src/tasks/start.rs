use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;

use super::SessionTask;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        self.start_task(turn_context, input, task).await;
    }
}
