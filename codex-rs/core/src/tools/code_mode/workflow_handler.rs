//! Dynamic Workflow execution and the model-callable saved-workflow launcher.
//!
//! Workflow source is host-authored: the model-visible `workflow_run` function
//! resolves an exact saved metadata name and never accepts inline JavaScript or
//! an arbitrary path. Runs share the code-mode service while keeping their
//! journal, budget, topology, and nested-run lineage in host-owned state.

mod admission;
mod bounds;
mod ledger;
mod lifecycle;
mod nested;
mod resume;
mod source;
mod start;

use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

use super::ExecContext;
use super::WORKFLOW_TOOL_NAME;
use super::is_workflow_tool_name;
use super::workflow_entry::start_saved_workflow_for_turn;

use bounds::bound_model_error;
use bounds::ensure_model_workflow_args_within_bounds;
pub(super) use bounds::ensure_workflow_args_within_bounds;
pub(crate) use bounds::ensure_workflow_enabled;
pub(super) use bounds::protocol_budget_snapshot;
#[cfg(test)]
pub(crate) use bounds::validate_workflow_meta;
pub(crate) use ledger::WorkflowRunLedger;
pub(crate) use ledger::WorkflowRunLineage;
#[allow(
    unused_imports,
    reason = "preserve the workflow_handler crate-visible API after extraction"
)]
pub(crate) use ledger::WorkflowRunLink;
#[allow(
    unused_imports,
    reason = "preserve the workflow_handler module-visible API after extraction"
)]
pub(super) use ledger::WorkflowRunTerminalFacts;
#[allow(
    unused_imports,
    reason = "preserve the workflow_handler crate-visible API after extraction"
)]
pub(crate) use lifecycle::WorkflowRunOutput;
pub(crate) use nested::run_workflow_by_name;
#[allow(
    unused_imports,
    reason = "preserve the workflow_handler crate-visible API after extraction"
)]
pub(crate) use resume::ResumeSeed;
pub(crate) use resume::WorkflowReplayAccess;
pub(crate) use resume::WorkflowResumeInvocation;
pub(crate) use resume::prepare_paused_workflow_resume;
pub(crate) use resume::prepare_workflow_prefix_replay;
pub(crate) use resume::resume_workflow_source_to_terminal;
pub(super) use source::read_named_workflow_source_bounded;
pub(super) use source::read_workflow_source_bounded;
pub(crate) use start::run_workflow_source_to_terminal;
pub(crate) use start::start_workflow_source;

/// Parse the model-authored workflow tool payload at the bounded error boundary.
fn parse_workflow_run_args(arguments: &str) -> Result<WorkflowRunArgs, FunctionCallError> {
    if arguments.len() > codex_code_mode::WORKFLOW_MODEL_CALL_MAX_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "{WORKFLOW_TOOL_NAME} arguments exceed the {}-byte model-context cap",
            codex_code_mode::WORKFLOW_MODEL_CALL_MAX_BYTES
        )));
    }
    serde_json::from_str::<WorkflowRunArgs>(arguments)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse {WORKFLOW_TOOL_NAME} arguments: {error}"
            ))
        })
        .map_err(bound_model_error)
}

pub(crate) struct CodeModeWorkflowHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<ToolSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowRunArgs {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
    resume_from_run_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowRunResult {
    run_id: String,
    status: &'static str,
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
        args: WorkflowRunArgs,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        ensure_model_workflow_args_within_bounds(&args.args)?;
        let exec = ExecContext { session, turn };
        let enabled_tools =
            codex_tools::collect_code_mode_tool_definitions(&self.nested_tool_specs);
        let run_id = start_saved_workflow_for_turn(
            exec,
            call_id,
            enabled_tools,
            &args.name,
            args.args,
            args.resume_from_run_id.as_deref(),
        )
        .await?;
        let result = WorkflowRunResult {
            run_id,
            status: "running",
        };
        let text = serde_json::to_string(&result).map_err(|error| {
            FunctionCallError::Fatal(format!(
                "failed to serialize {WORKFLOW_TOOL_NAME} result: {error}"
            ))
        })?;
        Ok(FunctionToolOutput::from_text(text, Some(true)))
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

        match payload {
            ToolPayload::Function { arguments } if is_workflow_tool_name(&tool_name) => {
                let args = parse_workflow_run_args(&arguments)?;
                self.execute(session, turn, call_id, args)
                    .await
                    .map(boxed_tool_output)
                    .map_err(bound_model_error)
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WORKFLOW_TOOL_NAME} expects JSON arguments"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeWorkflowHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[cfg(test)]
#[path = "workflow_handler/adapter_tests.rs"]
mod adapter_tests;

#[cfg(test)]
#[path = "workflow_background_tests.rs"]
mod background_tests;
