//! One-shot execution of an immutable saved-workflow source snapshot.

use codex_code_mode::CellId;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use codex_code_mode::StartedCell;
use codex_code_mode::StartedCellBinding;
use codex_code_mode::WaitOutcome;
use codex_core_workflows::WorkflowSourceResolver;
use codex_features::Feature;
use codex_rollout_trace::CodeCellTraceContext;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::warn;

use crate::function_tool::FunctionCallError;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::ExecContext;
use super::delegate::CodeModeDispatchBroker;

const SOURCE_RESOLVER_UNAVAILABLE: &str = "saved workflow discovery is unavailable";
const SOURCE_NOT_FOUND: &str = "saved workflow was not found";
const SOURCE_LOAD_FAILED: &str = "saved workflow source is unavailable";

/// Resolve one saved workflow and retain ownership of its runtime cell through completion.
///
/// Source capture completes before the code-mode service can initialize a session. The exact
/// captured bytes are then used for both execution and tracing; this path never reopens the
/// snapshot metadata path. This private staging seam is not exposed through a model, CLI, or
/// app-server entrypoint until its real-host and failure-path gates are complete.
pub(crate) async fn run_saved_workflow_once(
    exec: &ExecContext,
    call_id: &str,
    name: &str,
) -> Result<RuntimeResponse, FunctionCallError> {
    if !exec.turn.config.features.enabled(Feature::Workflow) {
        return Err(respond_to_model(SOURCE_RESOLVER_UNAVAILABLE));
    }
    codex_code_mode::ensure_workflow_name(name).map_err(FunctionCallError::RespondToModel)?;

    let resolver = exec
        .session
        .services
        .thread_extension_data
        .get::<WorkflowSourceResolver>()
        .ok_or_else(|| respond_to_model(SOURCE_RESOLVER_UNAVAILABLE))?;
    let snapshot = resolver
        .source_snapshot_by_name(name)
        .await
        .map_err(|error| {
            warn!(
                workflow_name = name,
                diagnostic = %error.diagnostic(),
                "failed to capture saved workflow source"
            );
            respond_to_model(SOURCE_LOAD_FAILED)
        })?
        .ok_or_else(|| respond_to_model(SOURCE_NOT_FOUND))?;
    if snapshot.metadata().name != name {
        warn!(
            workflow_name = name,
            resolved_name = snapshot.metadata().name,
            "saved workflow resolver returned a mismatched source snapshot"
        );
        return Err(respond_to_model(SOURCE_LOAD_FAILED));
    }

    let source = snapshot.source().to_string();
    let runtime_session = exec
        .session
        .services
        .code_mode_service
        .session()
        .await
        .map_err(|error| runtime_error("initialize", error))?;
    let runtime_task = exec
        .session
        .services
        .code_mode_service
        .reserve_runtime_task()
        .map_err(|error| runtime_error("reserve owner", error))?;
    let cancellation = runtime_task.cancellation_token();
    let dispatch_broker = Arc::clone(&exec.session.services.code_mode_service.dispatch_broker);
    let execute = Arc::clone(&runtime_session).execute_bound(ExecuteRequest {
        tool_call_id: call_id.to_string(),
        enabled_tools: Vec::new(),
        source: source.clone(),
        output_policy: ExecuteOutputPolicy::SavedWorkflow,
        yield_time_ms: None,
        max_output_tokens: None,
    });
    tokio::pin!(execute);
    let bound_started_cell = tokio::select! {
        // Claim a delivered cell before honoring shutdown so its exact owner can clean it up.
        biased;
        result = &mut execute => result.map_err(|error| runtime_error("start", error))?,
        _ = cancellation.cancelled() => {
            return Err(respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED));
        }
    };
    let (started_cell, binding) = bound_started_cell.into_parts();
    let cell_id = started_cell.cell_id.clone();
    let trace = exec
        .session
        .services
        .rollout_thread_trace
        .start_function_code_cell_trace(
            exec.turn.sub_id.as_str(),
            cell_id.as_str(),
            call_id,
            source,
        );
    let owner = RuntimeCellOwner {
        binding,
        dispatch_broker,
        cell_id: cell_id.clone(),
        trace,
        content_items: Vec::new(),
        ended: false,
    };
    owner.dispatch_broker.mark_cell_ready_for_dispatch(&cell_id);

    let (result_tx, result_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _runtime_task = runtime_task;
        let mut owner = owner;
        let result = tokio::select! {
            // Session shutdown wins simultaneous completion so a closed runtime cannot report
            // a newly successful workflow result.
            biased;
            _ = cancellation.cancelled() => Err(respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED)),
            result = observe_to_terminal(&mut owner, started_cell) => result,
        };
        if result.is_err() {
            owner.terminate_after_failure().await;
            owner.record_failed();
        }
        drop(owner);
        let _ = result_tx.send(result);
    });
    result_rx.await.map_err(|_| {
        warn!("saved workflow runtime owner stopped before reporting completion");
        respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED)
    })?
}

async fn observe_to_terminal(
    owner: &mut RuntimeCellOwner,
    started_cell: StartedCell,
) -> Result<RuntimeResponse, FunctionCallError> {
    let mut response = started_cell
        .initial_response()
        .await
        .map_err(|error| post_start_runtime_error("initial response", error))?;
    if response_cell_id(&response) != &owner.cell_id {
        warn!("saved workflow runtime returned a mismatched cell identity");
        return Err(respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED));
    }
    owner.trace.record_initial_response(&response);

    loop {
        if response_cell_id(&response) != &owner.cell_id {
            warn!("saved workflow runtime returned a mismatched cell identity");
            return Err(respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED));
        }
        match response {
            RuntimeResponse::Yielded {
                content_items,
                cell_id: _,
            } => {
                owner.content_items.extend(content_items);
                response = match owner
                    .binding
                    .wait(DEFAULT_WAIT_YIELD_TIME_MS)
                    .await
                    .map_err(|error| post_start_runtime_error("wait", error))?
                {
                    WaitOutcome::LiveCell(response) => response,
                    WaitOutcome::MissingCell(response) => {
                        if response_cell_id(&response) != &owner.cell_id {
                            warn!("saved workflow runtime returned a mismatched cell identity");
                        } else {
                            warn!("saved workflow runtime cell disappeared before completion");
                        }
                        return Err(respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED));
                    }
                };
            }
            RuntimeResponse::Terminated { .. } | RuntimeResponse::Result { .. } => {
                owner.record_ended(&response);
                return Ok(owner.with_accumulated_output(response));
            }
        }
    }
}

struct RuntimeCellOwner {
    binding: Arc<dyn StartedCellBinding>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    cell_id: CellId,
    trace: CodeCellTraceContext,
    content_items: Vec<codex_code_mode::FunctionCallOutputContentItem>,
    ended: bool,
}

impl RuntimeCellOwner {
    async fn terminate_after_failure(&self) {
        match self.binding.terminate().await {
            Ok(WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response)) => {
                if response_cell_id(&response) != &self.cell_id {
                    warn!("saved workflow cleanup returned a mismatched cell identity");
                } else if matches!(response, RuntimeResponse::Yielded { .. }) {
                    warn!("saved workflow cleanup did not reach a terminal state");
                }
            }
            Err(error) => {
                warn!(
                    diagnostic = %error,
                    "failed to terminate saved workflow runtime cell"
                );
            }
        }
    }

    fn record_failed(&mut self) {
        self.record_ended(&RuntimeResponse::Result {
            cell_id: self.cell_id.clone(),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        });
    }

    fn record_ended(&mut self, response: &RuntimeResponse) {
        self.trace.record_ended(response);
        self.ended = true;
    }

    fn with_accumulated_output(&mut self, response: RuntimeResponse) -> RuntimeResponse {
        match response {
            RuntimeResponse::Terminated {
                cell_id,
                mut content_items,
            } => {
                self.content_items.append(&mut content_items);
                RuntimeResponse::Terminated {
                    cell_id,
                    content_items: std::mem::take(&mut self.content_items),
                }
            }
            RuntimeResponse::Result {
                cell_id,
                mut content_items,
                error_text,
            } => {
                self.content_items.append(&mut content_items);
                RuntimeResponse::Result {
                    cell_id,
                    content_items: std::mem::take(&mut self.content_items),
                    error_text,
                }
            }
            RuntimeResponse::Yielded { .. } => {
                unreachable!("terminal workflow observer received a yielded response")
            }
        }
    }
}

impl Drop for RuntimeCellOwner {
    fn drop(&mut self) {
        if !self.ended {
            self.trace.record_ended(&RuntimeResponse::Result {
                cell_id: self.cell_id.clone(),
                content_items: Vec::new(),
                error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
            });
        }
        self.dispatch_broker.close_cell(&self.cell_id);
    }
}

fn response_cell_id(response: &RuntimeResponse) -> &CellId {
    match response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}

fn runtime_error(stage: &'static str, error: String) -> FunctionCallError {
    if error == SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE {
        return respond_to_model(SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE);
    }
    warn!(stage, diagnostic = %error, "saved workflow runtime failed");
    respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED)
}

fn post_start_runtime_error(stage: &'static str, error: String) -> FunctionCallError {
    warn!(stage, diagnostic = %error, "saved workflow runtime failed");
    respond_to_model(SAVED_WORKFLOW_EXECUTION_FAILED)
}

fn respond_to_model(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.to_string())
}

#[cfg(test)]
#[path = "saved_workflow_run_once_tests.rs"]
mod tests;
