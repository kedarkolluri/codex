use std::sync::Arc;

use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::WarningEvent;

use super::SessionTask;
use super::start_transaction::TaskStartOutcome;
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
        // Keep the admission lane across replacement and exact reservation so another start cannot
        // enter between them. Abort lifecycle contributors must not synchronously wait for a new
        // turn start while this replacement is in progress.
        let admission_permit = self.turn_start_gate.acquire_start_permit().await;
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        match self
            .start_task_with_admission_permit(
                Arc::clone(&turn_context),
                input,
                task,
                admission_permit,
            )
            .await
        {
            TaskStartOutcome::Started => {}
            TaskStartOutcome::Cancelled(reason) => {
                tracing::trace!(?reason, "task start cancelled before commit");
            }
            TaskStartOutcome::AtCapacity(waiter) => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(
                        waiter
                            .into_limit_error()
                            .to_error_event(/*message_prefix*/ None),
                    ),
                )
                .await;
            }
            TaskStartOutcome::Busy
            | TaskStartOutcome::StartInProgress(_)
            | TaskStartOutcome::PendingTriggerTurn => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(ErrorEvent {
                        message: "turn could not start because the session task slot is busy"
                            .to_string(),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                )
                .await;
            }
            TaskStartOutcome::Poisoned => {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Warning(WarningEvent {
                        message: "turn start failed closed after an internal lifecycle mismatch"
                            .to_string(),
                    }),
                )
                .await;
            }
        }
    }
}
