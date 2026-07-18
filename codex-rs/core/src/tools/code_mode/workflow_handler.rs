//! Workflow host tool skeleton (P0-host-tool-skeleton).
//!
//! This is a clone of [`super::execute_handler::CodeModeExecuteHandler`] adapted
//! to run a *workflow* script instead of a model-authored code-mode `exec`
//! program. The only structural differences from plain code-mode exec are:
//!
//! 1. The handler is gated behind [`Feature::Workflow`] — it is unreachable
//!    unless the feature is enabled.
//! 2. Before touching the isolate the raw source is validated with
//!    [`codex_code_mode::parse_workflow_meta`]; a script without a valid static
//!    `export const meta = { ... }` manifest is rejected up front, so no isolate
//!    execution ever runs for an invalid workflow.
//! 3. The validated body is then submitted to a *fresh* code-mode isolate via
//!    the shared `code_mode_service.execute` / `run_runtime` path and its
//!    top-level result is returned.
//!
//! Everything downstream of "run the body once" — `agent()`, journal, budget,
//! worktree isolation, and determinism hardening — is intentionally deferred to
//! later phases. This skeleton shares the code-mode service and does NOT fork
//! the runtime, so existing code-mode behavior is unaffected.

use std::time::Instant;

use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_features::Feature;
use codex_features::Features;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

use super::CodeModeService;
use super::ExecContext;
use super::WORKFLOW_TOOL_NAME;
use super::handle_runtime_response;
use super::is_workflow_tool_name;

/// Hard cap (in bytes) on any error string this handler surfaces to the model
/// as a tool result.
///
/// Defense-in-depth: even though [`codex_code_mode::parse_workflow_meta`] now
/// bounds the identifiers/numbers it echoes, an error can still originate from
/// several layers (the exec-source parser, the isolate service, the runtime
/// response adapter). No single error returned from the workflow tool path
/// should be able to balloon the model context, so every model-visible error is
/// hard-truncated at a UTF-8 boundary with a marker.
const MAX_MODEL_ERROR_BYTES: usize = 2048;

/// Marker appended to a truncated error. Its byte length is reserved inside the
/// [`MAX_MODEL_ERROR_BYTES`] budget so the final string never exceeds the cap.
const ERROR_TRUNCATION_MARKER: &str = "… [error truncated]";

/// Truncate a model-visible error message so its total length never exceeds the
/// hard cap [`MAX_MODEL_ERROR_BYTES`]. The marker length is reserved inside the
/// budget (truncate to `cap - marker_len` at a UTF-8 char boundary), so the
/// returned string — prefix plus marker — is guaranteed `<= MAX_MODEL_ERROR_BYTES`.
fn truncate_model_error(message: String) -> String {
    if message.len() <= MAX_MODEL_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_MODEL_ERROR_BYTES.saturating_sub(ERROR_TRUNCATION_MARKER.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ERROR_TRUNCATION_MARKER}", &message[..end])
}

/// Hard-bound the runtime error text carried by a successful workflow run before
/// it reaches the model.
///
/// A script failure does NOT come back as [`FunctionCallError`]; it returns as
/// `Ok(RuntimeResponse::Result { error_text: Some(..) })`, which
/// [`handle_runtime_response`] renders into model-visible output subject only to
/// the workflow's *own* `max_output_tokens` budget (which the script can raise).
/// That bypasses [`bound_model_error`], so the same [`MAX_MODEL_ERROR_BYTES`] cap
/// must be applied here independently, before the response is rendered.
fn bound_runtime_error(response: &mut RuntimeResponse) {
    if let RuntimeResponse::Result {
        error_text: Some(text),
        ..
    } = response
    {
        let bounded = truncate_model_error(std::mem::take(text));
        *text = bounded;
    }
}

/// Hard-bound a [`FunctionCallError`] before it reaches the model. Only the
/// message payload is truncated; the error variant is preserved.
fn bound_model_error(error: FunctionCallError) -> FunctionCallError {
    match error {
        FunctionCallError::RespondToModel(message) => {
            FunctionCallError::RespondToModel(truncate_model_error(message))
        }
        FunctionCallError::Fatal(message) => {
            FunctionCallError::Fatal(truncate_model_error(message))
        }
    }
}

/// Result of running a workflow body once in a fresh isolate, carrying the
/// bits `handle_runtime_response` needs to render the model-facing output.
#[derive(Debug)]
pub(crate) struct WorkflowRunOutput {
    pub(crate) response: RuntimeResponse,
    pub(crate) max_output_tokens: Option<usize>,
    pub(crate) started_at: Instant,
}

/// Reject the call unless the `workflow` feature is enabled. This is what makes
/// the handler unreachable when [`Feature::Workflow`] is off.
pub(crate) fn ensure_workflow_enabled(features: &Features) -> Result<(), FunctionCallError> {
    if features.enabled(Feature::Workflow) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "`{WORKFLOW_TOOL_NAME}` requires the `workflow` feature to be enabled"
        )))
    }
}

/// Validate the leading `export const meta = { ... }` manifest WITHOUT executing
/// (or even reading past) the workflow body. A script with a missing or invalid
/// `meta` is rejected here, before any isolate execution.
pub(crate) fn validate_workflow_meta(code: &str) -> Result<(), FunctionCallError> {
    codex_code_mode::parse_workflow_meta(code)
        .map(|_meta| ())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid workflow `meta` manifest: {err}"))
        })
}

/// Feature-gate, parse+validate the manifest, then run the workflow body exactly
/// once in a fresh code-mode isolate via the shared service.
///
/// Shared by the [`CodeModeWorkflowHandler`] tool path and the integration test
/// so both drive the identical run-body-once sequence.
pub(crate) async fn run_workflow_source(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
) -> Result<WorkflowRunOutput, FunctionCallError> {
    ensure_workflow_enabled(features)?;

    // The workflow source is admitted identically to any code-mode program: an
    // optional leading `// @exec:` pragma followed by the ES-module body.
    let args =
        codex_code_mode::parse_exec_source(source).map_err(FunctionCallError::RespondToModel)?;

    // Reject scripts without a valid static `meta` manifest before we ever touch
    // the isolate.
    validate_workflow_meta(&args.code)?;

    let started_at = Instant::now();
    let started_cell = service
        .execute(codex_code_mode::ExecuteRequest {
            tool_call_id: call_id,
            enabled_tools,
            source: args.code.clone(),
            yield_time_ms: args.yield_time_ms,
            max_output_tokens: args.max_output_tokens,
            // Explicit workflow invocation mode: authorizes the workflow-only
            // narrator globals for this fresh isolate. Plain code-mode exec
            // leaves this `false`.
            workflow: true,
        })
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let cell_id = started_cell.cell_id.clone();
    service.mark_cell_ready_for_dispatch(&cell_id);
    let response = started_cell
        .initial_response()
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    // Yielded cells keep running; the terminal lifecycle is only closed here when
    // the first response also ended the runtime.
    if !matches!(response, RuntimeResponse::Yielded { .. }) {
        service.finish_cell_dispatch(&cell_id);
    }

    Ok(WorkflowRunOutput {
        response,
        max_output_tokens: args.max_output_tokens,
        started_at,
    })
}

pub(crate) struct CodeModeWorkflowHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<ToolSpec>,
}

impl CodeModeWorkflowHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<ToolSpec>) -> Self {
        Self {
            spec,
            nested_tool_specs,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        source: String,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let exec = ExecContext { session, turn };
        let enabled_tools =
            codex_tools::collect_code_mode_tool_definitions(&self.nested_tool_specs);
        let mut output = run_workflow_source(
            exec.turn.config.features.get(),
            &exec.session.services.code_mode_service,
            call_id,
            enabled_tools,
            &source,
        )
        .await?;
        // Script failures return on the `Ok` path as `RuntimeResponse::Result`
        // carrying `error_text`; bound it here so a workflow cannot raise its own
        // `max_output_tokens` to smuggle an unbounded error into the model output.
        bound_runtime_error(&mut output.response);
        exec.session.services.elicitations.wait_until_clear().await;
        handle_runtime_response(
            &exec,
            output.response,
            output.max_output_tokens,
            output.started_at,
        )
        .await
        .map_err(FunctionCallError::RespondToModel)
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeWorkflowHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeWorkflowHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        // `handle_call` is the model-visible boundary for the workflow tool: every
        // error returned here is rendered into a tool result. Hard-bound the
        // message so no layer (meta parser, exec-source parser, isolate service,
        // response adapter) can surface an unbounded string to the model.
        match payload {
            ToolPayload::Custom { input } if is_workflow_tool_name(&tool_name) => self
                .execute(session, turn, call_id, input)
                .await
                .map(boxed_tool_output)
                .map_err(bound_model_error),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WORKFLOW_TOOL_NAME} expects raw workflow JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeWorkflowHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::ERROR_TRUNCATION_MARKER;
    use super::FunctionCallError;
    use super::MAX_MODEL_ERROR_BYTES;
    use super::RuntimeResponse;
    use super::bound_model_error;
    use super::bound_runtime_error;
    use super::truncate_model_error;
    use codex_code_mode::CellId;

    #[test]
    fn short_error_is_unchanged() {
        let message = "invalid workflow `meta` manifest: boom".to_string();
        assert_eq!(truncate_model_error(message.clone()), message);
    }

    #[test]
    fn oversized_error_is_hard_truncated_with_marker() {
        let message = "z".repeat(MAX_MODEL_ERROR_BYTES * 4);
        let truncated = truncate_model_error(message);
        assert!(
            truncated.ends_with(ERROR_TRUNCATION_MARKER),
            "expected truncation marker, got: {truncated}"
        );
        // The marker length is reserved inside the budget, so the final string —
        // prefix plus marker — never exceeds the hard cap.
        assert!(
            truncated.len() <= MAX_MODEL_ERROR_BYTES,
            "truncated error is {} bytes, over the {MAX_MODEL_ERROR_BYTES}-byte cap",
            truncated.len()
        );
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // A multi-byte char straddling the cap must not panic or split a code
        // point: build a string of 3-byte chars whose boundary is not aligned to
        // the byte cap.
        let message = "€".repeat(MAX_MODEL_ERROR_BYTES);
        let truncated = truncate_model_error(message);
        // Round-trips as valid UTF-8 (implicitly, since it is a `String`) and is
        // bounded by the hard cap (marker included).
        assert!(truncated.len() <= MAX_MODEL_ERROR_BYTES);
        assert!(truncated.ends_with(ERROR_TRUNCATION_MARKER));
    }

    #[test]
    fn bound_runtime_error_caps_result_error_text() {
        // Script failures arrive on the `Ok` path as `RuntimeResponse::Result`
        // carrying an unbounded `error_text`. Bounding must cap it at the same
        // hard limit so a workflow cannot smuggle a huge error to the model.
        let huge = "q".repeat(MAX_MODEL_ERROR_BYTES * 8);
        let mut response = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: Some(huge),
        };
        bound_runtime_error(&mut response);
        match response {
            RuntimeResponse::Result { error_text, .. } => {
                let text = error_text.expect("error text preserved");
                assert!(
                    text.len() <= MAX_MODEL_ERROR_BYTES,
                    "model-visible error text is {} bytes, over the cap",
                    text.len()
                );
                assert!(text.ends_with(ERROR_TRUNCATION_MARKER));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn bound_runtime_error_leaves_short_error_and_success_untouched() {
        let short = "boom".to_string();
        let mut response = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: Some(short.clone()),
        };
        bound_runtime_error(&mut response);
        match &response {
            RuntimeResponse::Result { error_text, .. } => {
                assert_eq!(error_text.as_deref(), Some(short.as_str()));
            }
            other => panic!("expected Result, got {other:?}"),
        }

        // A successful result (no error text) is left as-is.
        let mut ok = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: None,
        };
        bound_runtime_error(&mut ok);
        match ok {
            RuntimeResponse::Result { error_text, .. } => assert!(error_text.is_none()),
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn bound_model_error_preserves_variant_and_truncates() {
        let long = "y".repeat(MAX_MODEL_ERROR_BYTES * 2);
        match bound_model_error(FunctionCallError::RespondToModel(long.clone())) {
            FunctionCallError::RespondToModel(message) => {
                assert!(message.ends_with("… [error truncated]"));
                assert!(message.len() < long.len());
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
        match bound_model_error(FunctionCallError::Fatal(long)) {
            FunctionCallError::Fatal(message) => {
                assert!(message.ends_with("… [error truncated]"));
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }
}
