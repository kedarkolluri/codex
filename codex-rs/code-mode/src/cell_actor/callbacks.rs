use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::AgentSpawnOutcome;
use futures::FutureExt;
use serde_json::Value as JsonValue;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::CellHost;
use super::CellToolCall;
use crate::TaskFailureHandler;
use crate::runtime::RuntimeCommand;

#[derive(Clone, Copy)]
pub(super) enum CallbackCompletion {
    DrainNotifications,
    Cancel,
}

pub(super) fn spawn_notification<H: CellHost>(
    tasks: &mut JoinSet<()>,
    host: Arc<H>,
    call_id: String,
    text: String,
    cancellation_token: CancellationToken,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    tasks.spawn(async move {
        let callback =
            AssertUnwindSafe(async move { host.notify(call_id, text, cancellation_token).await })
                .catch_unwind()
                .await;
        match callback {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!("failed to deliver code mode notification: {err}"),
            Err(_) => report_task_failure(
                task_failure_handler.as_ref(),
                "code mode notification task panicked".to_string(),
            ),
        }
    });
}

pub(super) fn spawn_tool<H: CellHost>(
    tasks: &mut JoinSet<()>,
    host: Arc<H>,
    invocation: CellToolCall,
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    cancellation_token: CancellationToken,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    tasks.spawn(async move {
        let id = invocation.id.clone();
        let callback =
            AssertUnwindSafe(async move { host.invoke_tool(invocation, cancellation_token).await })
                .catch_unwind()
                .await;
        let (command, failure_reason) = match callback {
            Ok(Ok(result)) => (RuntimeCommand::ToolResponse { id, result }, None),
            Ok(Err(error_text)) => (RuntimeCommand::ToolError { id, error_text }, None),
            Err(_) => {
                let failure_reason = "code mode tool task panicked".to_string();
                (
                    RuntimeCommand::ToolError {
                        id,
                        error_text: failure_reason.clone(),
                    },
                    Some(failure_reason),
                )
            }
        };
        let _ = runtime_tx.send(command);
        if let Some(failure_reason) = failure_reason {
            report_task_failure(task_failure_handler.as_ref(), failure_reason);
        }
    });
}

/// Route a workflow `agent(prompt, opts?)` spawn request to the host and settle the isolate promise
/// by id, mirroring [`spawn_tool`].
///
/// Each `agent()` call gets its own independent task in `tasks` (the shared tool JoinSet), so N
/// concurrent `agent()` calls resolve independently and out-of-order — nothing here serializes them.
/// The host's [`CellHost::spawn_agent`] returns an [`AgentSpawnOutcome`], which maps to one of two
/// runtime commands, exactly reusing the tool-callback resolve/reject paths:
/// - `Completed(value)` -> [`RuntimeCommand::ToolResponse`] carrying the host-marshaled JSON (a
///   string when schemaless, or the validated `opts.schema` object), forwarded verbatim to
///   `json_to_v8`.
/// - `Failed` (death-is-null: a dead/aborted agent, or a schema parse/validation failure resolved
///   host-side, or a panicked host task) -> [`RuntimeCommand::ToolResponse`] with JS `null`.
/// - `Rejected(message)` -> [`RuntimeCommand::ToolError`], the same path a failed nested tool takes,
///   so the isolate *rejects* (throws) the `agent()` promise with `message` (e.g. the 1001st
///   `agent()`/over-budget call surfacing `AgentCapReached`/`BudgetExceeded`).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_agent<H: CellHost>(
    tasks: &mut JoinSet<()>,
    host: Arc<H>,
    id: String,
    prompt: String,
    ordinal: u64,
    opts: AgentCallOpts,
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    cancellation_token: CancellationToken,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    tasks.spawn(async move {
        let outcome = AssertUnwindSafe(async move {
            host.spawn_agent(prompt, ordinal, opts, cancellation_token)
                .await
        })
        .catch_unwind()
        .await;
        let (command, failure_reason) = match outcome {
            Ok(AgentSpawnOutcome::Completed(value)) => {
                (RuntimeCommand::ToolResponse { id, result: value }, None)
            }
            // Death-is-null: a failed/aborted agent resolves the promise to JS `null`, never throws.
            Ok(AgentSpawnOutcome::Failed) => (
                RuntimeCommand::ToolResponse {
                    id,
                    result: JsonValue::Null,
                },
                None,
            ),
            // Admission-time cap/budget rejection: reject (throw) the promise via the tool-error
            // path so the isolate surfaces `message` instead of resolving `null`.
            Ok(AgentSpawnOutcome::Rejected(message)) => (
                RuntimeCommand::ToolError {
                    id,
                    error_text: message,
                },
                None,
            ),
            // A panicked host spawn task is a host failure, not an agent failure; keep the
            // never-throws contract by still resolving the promise to `null`, but surface the panic.
            Err(_) => (
                RuntimeCommand::ToolResponse {
                    id,
                    result: JsonValue::Null,
                },
                Some("code mode agent spawn task panicked".to_string()),
            ),
        };
        let _ = runtime_tx.send(command);
        if let Some(failure_reason) = failure_reason {
            report_task_failure(task_failure_handler.as_ref(), failure_reason);
        }
    });
}

pub(super) async fn finish_callbacks(
    cancellation_token: &CancellationToken,
    notification_tasks: &mut JoinSet<()>,
    tool_tasks: &mut JoinSet<()>,
    completion: CallbackCompletion,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    if matches!(completion, CallbackCompletion::Cancel) {
        cancellation_token.cancel();
    }
    drain_tasks(notification_tasks, "notification", task_failure_handler).await;
    cancellation_token.cancel();
    drain_tasks(tool_tasks, "tool", task_failure_handler).await;
}

pub(super) fn report_task_result(
    task_result: Option<Result<(), tokio::task::JoinError>>,
    description: &str,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    if let Some(Err(err)) = task_result
        && !err.is_cancelled()
    {
        report_task_failure(
            task_failure_handler,
            format!("code mode {description} task failed: {err}"),
        );
    }
}

fn report_task_failure(task_failure_handler: Option<&TaskFailureHandler>, failure_reason: String) {
    warn!("{failure_reason}");
    if let Some(task_failure_handler) = task_failure_handler {
        task_failure_handler(failure_reason);
    }
}

async fn drain_tasks(
    tasks: &mut JoinSet<()>,
    description: &str,
    task_failure_handler: Option<&TaskFailureHandler>,
) {
    while let Some(result) = tasks.join_next().await {
        report_task_result(Some(result), description, task_failure_handler);
    }
}

#[cfg(test)]
#[path = "callbacks_tests.rs"]
mod tests;
