//! Thread-scoped entrypoints for starting saved workflows outside a model turn.

use std::sync::Arc;

use codex_code_mode::ToolDefinition;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn::built_tools;
use crate::session::turn_context::TurnContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::turn_diff_tracker::TurnDiffTracker;

use super::ExecContext;
use super::workflow_handler::WorkflowReplayAccess;
use super::workflow_handler::WorkflowResumeInvocation;
use super::workflow_handler::WorkflowRunLineage;
use super::workflow_handler::ensure_workflow_args_within_bounds;
use super::workflow_handler::ensure_workflow_enabled;
use super::workflow_handler::prepare_paused_workflow_resume;
use super::workflow_handler::prepare_workflow_prefix_replay;
use super::workflow_handler::read_named_workflow_source_bounded;
use super::workflow_handler::start_workflow_source;
use super::workflow_progress::WorkflowEventTarget;

/// Resolve and durably start a saved workflow using the thread's effective
/// configuration, provider, router, environment, and agent-control graph.
pub(crate) async fn start_saved_workflow(
    session: &Arc<Session>,
    name: &str,
    args: serde_json::Value,
) -> CodexResult<String> {
    let turn = session.new_default_turn().await;
    if !turn.config.features.enabled(Feature::Workflow) {
        return Err(CodexErr::InvalidRequest(
            "the `workflow` feature must be enabled to start a workflow".to_string(),
        ));
    }
    let step_context = session.capture_step_context(Arc::clone(&turn)).await;
    let cancellation = CancellationToken::new();
    let router = built_tools(session, step_context.as_ref(), &cancellation).await?;
    let tracker: SharedTurnDiffTracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let _turn_worker = session
        .services
        .code_mode_service
        .start_turn_worker(
            session,
            Arc::clone(&step_context),
            Arc::clone(&router),
            tracker,
        )
        .await
        .ok_or_else(|| {
            CodexErr::UnsupportedOperation(
                "workflow runtime could not start its thread-scoped dispatch worker".to_string(),
            )
        })?;
    let enabled_tools =
        codex_tools::collect_code_mode_tool_definitions(&router.model_visible_specs());
    start_saved_workflow_for_turn(
        ExecContext {
            session: Arc::clone(session),
            turn,
        },
        format!("workflow-start-{}", uuid::Uuid::now_v7()),
        enabled_tools,
        name,
        args,
        /*resume_from_run_id*/ None,
    )
    .await
    .map_err(workflow_start_error)
}

/// Resume a paused workflow using only its immutable durable artifacts.
pub(crate) async fn resume_saved_workflow(
    session: &Arc<Session>,
    source_run_id: &str,
) -> CodexResult<String> {
    let turn = session.new_default_turn().await;
    if !turn.config.features.enabled(Feature::Workflow) {
        return Err(CodexErr::InvalidRequest(
            "the `workflow` feature must be enabled to resume a workflow".to_string(),
        ));
    }
    let step_context = session.capture_step_context(Arc::clone(&turn)).await;
    let cancellation = CancellationToken::new();
    let router = built_tools(session, step_context.as_ref(), &cancellation).await?;
    let tracker: SharedTurnDiffTracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let _turn_worker = session
        .services
        .code_mode_service
        .start_turn_worker(
            session,
            Arc::clone(&step_context),
            Arc::clone(&router),
            tracker,
        )
        .await
        .ok_or_else(|| {
            CodexErr::UnsupportedOperation(
                "workflow runtime could not start its thread-scoped dispatch worker".to_string(),
            )
        })?;
    let enabled_tools =
        codex_tools::collect_code_mode_tool_definitions(&router.model_visible_specs());
    resume_saved_workflow_with_persisted_invocation_for_turn(
        ExecContext {
            session: Arc::clone(session),
            turn,
        },
        format!("workflow-resume-{}", uuid::Uuid::now_v7()),
        enabled_tools,
        source_run_id,
    )
    .await
    .map_err(workflow_start_error)
}

/// Resolve an exact saved workflow name and durably start it against an
/// already-active Session/Turn execution graph.
///
/// This is shared by the model-callable `workflow_run` tool and app-server's
/// thread-scoped `workflow/start` method. The caller supplies only host-built
/// tool definitions; source and paths always come from saved-workflow discovery.
pub(crate) async fn start_saved_workflow_for_turn(
    exec: ExecContext,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    name: &str,
    args: serde_json::Value,
    resume_from_run_id: Option<&str>,
) -> Result<String, FunctionCallError> {
    validate_saved_workflow_name(name)?;
    ensure_workflow_enabled(exec.turn.config.features.get())?;
    ensure_workflow_args_within_bounds(&args)?;
    if let Some(run_id) = resume_from_run_id {
        validate_run_id(run_id)?;
    }

    let source = resolve_saved_workflow_source(&exec, name).await?;

    let event_target = WorkflowEventTarget::session(exec.clone());
    if let Some(source_run_id) = resume_from_run_id {
        let owner_thread_id = event_target.owner_thread_id().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "workflow replay source is unavailable or not resumable".to_string(),
            )
        })?;
        let seed = prepare_workflow_prefix_replay(
            exec.turn.config.features.get(),
            &event_target,
            exec.turn.config.codex_home.as_path(),
            source_run_id,
            &source,
            &args,
            WorkflowReplayAccess::ThreadOwner(owner_thread_id),
        )
        .await?;
        return start_workflow_source(
            exec.turn.config.features.get(),
            &exec.session.services.code_mode_service,
            call_id,
            enabled_tools,
            &source,
            args,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            exec.turn.config.codex_home.as_path(),
            Some(seed),
            event_target,
        )
        .await;
    }

    start_workflow_source(
        exec.turn.config.features.get(),
        &exec.session.services.code_mode_service,
        call_id,
        enabled_tools,
        &source,
        args,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        exec.turn.config.codex_home.as_path(),
        /*resume*/ None,
        event_target,
    )
    .await
}

/// Resume a paused workflow using only its immutable script and host-private invocation artifact.
///
/// This is intentionally a separate entry point so callers never encode
/// persisted secret arguments into an ambiguous optional protocol field. It
/// does not consult the mutable saved-workflow registry: deleting or replacing
/// a saved source after pause cannot change the resumed program.
pub(crate) async fn resume_saved_workflow_with_persisted_invocation_for_turn(
    exec: ExecContext,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source_run_id: &str,
) -> Result<String, FunctionCallError> {
    ensure_workflow_enabled(exec.turn.config.features.get())?;
    validate_run_id(source_run_id)?;
    let source_paths = codex_workflow_journal::storage::WorkflowRunPaths::new(
        exec.turn.config.codex_home.as_path(),
        source_run_id,
    );
    let source = tokio::task::spawn_blocking(move || source_paths.read_script_bounded())
        .await
        .map_err(|_| persisted_resume_error())?
        .map_err(|_| persisted_resume_error())?;
    let event_target = WorkflowEventTarget::session(exec.clone());
    let prepared = prepare_paused_workflow_resume(
        exec.turn.config.features.get(),
        &event_target,
        exec.turn.config.codex_home.as_path(),
        source_run_id,
        &source,
        WorkflowResumeInvocation::Persisted,
    )
    .await?;
    let result = start_workflow_source(
        exec.turn.config.features.get(),
        &exec.session.services.code_mode_service,
        call_id,
        enabled_tools,
        &prepared.source,
        prepared.args.clone(),
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        exec.turn.config.codex_home.as_path(),
        Some(prepared.seed.clone()),
        event_target,
    )
    .await;
    drop(prepared);
    result
}

fn persisted_resume_error() -> FunctionCallError {
    FunctionCallError::RespondToModel(
        "workflow checkpoint is unavailable or not resumable".to_string(),
    )
}

async fn resolve_saved_workflow_source(
    exec: &ExecContext,
    name: &str,
) -> Result<String, FunctionCallError> {
    let project_cwd = project_workflow_root(exec.turn.as_ref());
    let roots = codex_core_workflows::workflow_roots(
        project_cwd.as_deref(),
        dirs::home_dir().as_deref(),
        Some(exec.turn.config.codex_home.as_path()),
    );
    let registry = codex_core_workflows::load_workflows_from_roots(roots).await;
    let metadata = registry.resolve_by_name(name).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "workflow `{name}` did not resolve to a saved workflow"
        ))
    })?;
    read_named_workflow_source_bounded(&metadata.path, name)
        .await
        .map_err(|error| {
            tracing::warn!(
                path = %metadata.path.display(),
                %error,
                "failed to read saved workflow source"
            );
            FunctionCallError::RespondToModel(format!("failed to read saved workflow `{name}`"))
        })
}

fn validate_saved_workflow_name(name: &str) -> Result<(), FunctionCallError> {
    codex_code_mode::ensure_workflow_name(name).map_err(FunctionCallError::RespondToModel)
}

fn validate_run_id(run_id: &str) -> Result<(), FunctionCallError> {
    let parsed = uuid::Uuid::parse_str(run_id).map_err(|_| persisted_resume_error())?;
    if parsed.to_string() != run_id {
        return Err(persisted_resume_error());
    }
    Ok(())
}

fn project_workflow_root(turn: &TurnContext) -> Option<codex_utils_absolute_path::AbsolutePathBuf> {
    turn.environments
        .primary()
        .filter(|environment| !environment.environment.is_remote())
        .and_then(|environment| environment.cwd().to_abs_path().ok())
}

fn workflow_start_error(error: FunctionCallError) -> CodexErr {
    match error {
        FunctionCallError::RespondToModel(message) => CodexErr::InvalidRequest(message),
        FunctionCallError::Fatal(message) => CodexErr::Fatal(message),
    }
}
