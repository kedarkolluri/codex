use codex_code_mode::CellId;
use codex_code_mode::RuntimeResponse;

use crate::tools::code_mode::ExecContext;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;

use super::bounds::truncate_model_error;
use super::ledger::WorkflowRunLineage;
use super::source::read_named_workflow_source_bounded;
use super::start::run_workflow_source_to_terminal;

/// The hard `workflow()` nesting ceiling: exactly one level, always.
const MAX_WORKFLOW_NESTING_DEPTH: i32 = 1;

fn admit_nested_workflow_depth(parent_depth: i32) -> Result<i32, i32> {
    use crate::agent::next_spawn_depth;

    let child_depth = next_spawn_depth(parent_depth);
    if child_depth > MAX_WORKFLOW_NESTING_DEPTH {
        Err(child_depth)
    } else {
        Ok(child_depth)
    }
}

/// Resolve and execute a saved workflow called from another workflow isolate.
pub(crate) async fn run_workflow_by_name(
    exec: &ExecContext,
    parent_cell_id: &CellId,
    name: &str,
    args: Option<serde_json::Value>,
    cancellation: tokio_util::sync::CancellationToken,
) -> codex_code_mode::AgentSpawnOutcome {
    use codex_code_mode::AgentSpawnOutcome;

    if cancellation.is_cancelled() {
        return AgentSpawnOutcome::Failed;
    }

    let service = &exec.session.services.code_mode_service;
    let ledger = service.workflow_run_ledger();
    let parent_run_id = ledger.parent_run_id_for_cell(parent_cell_id);
    let parent_depth = ledger.depth_for_cell(parent_cell_id).unwrap_or(0);
    let child_depth = match admit_nested_workflow_depth(parent_depth) {
        Ok(child_depth) => child_depth,
        Err(child_depth) => {
            return AgentSpawnOutcome::Rejected(format!(
                "workflow('{name}') exceeds the one-level nesting limit \
                 (depth {child_depth} > {MAX_WORKFLOW_NESTING_DEPTH}); \
                 workflow() may nest only one level deep"
            ));
        }
    };

    let cwd = exec
        .turn
        .environments
        .primary()
        .filter(|environment| !environment.environment.is_remote())
        .and_then(|environment| environment.cwd().to_abs_path().ok());
    let roots = codex_core_workflows::workflow_roots(
        cwd.as_deref(),
        dirs::home_dir().as_deref(),
        Some(exec.turn.config.codex_home.as_path()),
    );
    let registry = codex_core_workflows::load_workflows_from_roots(roots).await;
    let Some(metadata) = registry.resolve_by_name(name) else {
        return AgentSpawnOutcome::Rejected(format!(
            "workflow('{name}') did not resolve to a saved workflow in the registry"
        ));
    };

    let source = match read_named_workflow_source_bounded(metadata.path.as_path(), name).await {
        Ok(source) => source,
        Err(error) => {
            return AgentSpawnOutcome::Rejected(format!(
                "failed to read saved workflow '{name}': {error}"
            ));
        }
    };

    let args_value = args.unwrap_or(serde_json::Value::Null);
    if cancellation.is_cancelled() {
        return AgentSpawnOutcome::Failed;
    }

    let call_id = format!("workflow-nested-{}", uuid::Uuid::now_v7());
    match run_workflow_source_to_terminal(
        exec.turn.config.features.get(),
        service,
        call_id,
        Vec::new(),
        &source,
        args_value,
        WorkflowRunLineage {
            parent_run_id,
            depth: child_depth,
        },
        exec.turn.config.codex_home.as_path(),
        None,
        WorkflowEventTarget::nested(exec.clone(), parent_cell_id.clone()),
        cancellation,
    )
    .await
    {
        Ok(output) => workflow_result_outcome(output.response),
        Err(error) => AgentSpawnOutcome::Rejected(truncate_model_error(error.to_string())),
    }
}

/// Map a terminal nested workflow response onto the promise settlement outcome.
fn workflow_result_outcome(response: RuntimeResponse) -> codex_code_mode::AgentSpawnOutcome {
    use codex_code_mode::AgentSpawnOutcome;

    let (content_items, error_text) = match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (content_items, error_text),
        RuntimeResponse::Terminated { .. } => return AgentSpawnOutcome::Failed,
        RuntimeResponse::Yielded { .. } => {
            return AgentSpawnOutcome::Rejected(
                "nested workflow run did not complete (it yielded)".to_string(),
            );
        }
    };
    if let Some(error_text) = error_text {
        return AgentSpawnOutcome::Rejected(truncate_model_error(format!(
            "nested workflow run failed: {error_text}"
        )));
    }
    match join_result_text(&content_items) {
        Some(text) => {
            let value = serde_json::Value::String(text);
            match codex_workflow_journal::ensure_workflow_agent_return(&value) {
                Ok(()) => AgentSpawnOutcome::Completed(value),
                Err(error) => AgentSpawnOutcome::Rejected(truncate_model_error(format!(
                    "nested workflow result rejected: {error}"
                ))),
            }
        }
        None => AgentSpawnOutcome::Failed,
    }
}

fn join_result_text(
    content_items: &[codex_code_mode::FunctionCallOutputContentItem],
) -> Option<String> {
    let segments = content_items
        .iter()
        .filter_map(|item| match item {
            codex_code_mode::FunctionCallOutputContentItem::InputText { text }
                if !text.trim().is_empty() =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("\n"))
    }
}

#[cfg(test)]
#[path = "nested_tests.rs"]
mod tests;
