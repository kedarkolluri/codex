use std::path::PathBuf;
use std::sync::Arc;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::workflows_service::WorkflowsService;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentControlResponse;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowMetadata;
use codex_app_server_protocol::WorkflowPauseDisposition;
use codex_app_server_protocol::WorkflowPauseParams;
use codex_app_server_protocol::WorkflowPauseResponse;
use codex_app_server_protocol::WorkflowReadParams;
use codex_app_server_protocol::WorkflowReadResponse;
use codex_app_server_protocol::WorkflowResumeParams;
use codex_app_server_protocol::WorkflowResumeResponse;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowSaveDisposition;
use codex_app_server_protocol::WorkflowSaveParams;
use codex_app_server_protocol::WorkflowSaveResponse;
use codex_app_server_protocol::WorkflowSaveScope;
use codex_app_server_protocol::WorkflowScope;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowStopDisposition;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::ThreadWorkflowSaveError;
use codex_core::ThreadWorkflowSaveScope;
use codex_core::WorkflowAgentControlAction as CoreWorkflowAgentControlAction;
use codex_core::WorkflowAgentControlDisposition as CoreWorkflowAgentControlDisposition;
use codex_core::WorkflowPauseDisposition as CoreWorkflowPauseDisposition;
use codex_core::WorkflowStopDisposition as CoreWorkflowStopDisposition;
use codex_core_workflows::WORKFLOW_SOURCE_MAX_BYTES;
use codex_core_workflows::WorkflowRegistry;
use codex_core_workflows::WorkflowSaveError;
use codex_core_workflows::WorkflowSaveMode;
use codex_core_workflows::WorkflowSaveOutcome;
use codex_core_workflows::WorkflowScope as CoreWorkflowScope;
use codex_core_workflows::workflow_roots;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_rollout::StateDbHandle;
use codex_utils_path_uri::LegacyAppPathString;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus as JournalWorkflowRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use tracing::warn;

const WORKFLOW_LIST_DEFAULT_LIMIT: usize = 50;
const WORKFLOW_LIST_MAX_LIMIT: usize = 100;
const WORKFLOW_START_MAX_NAME_BYTES: usize = 256;
const WORKFLOW_START_MAX_ARGS_BYTES: usize = 32 * 1024;
const WORKFLOW_RUN_NOT_ACTIVE_MESSAGE: &str = "workflow run is not active for this thread";
const WORKFLOW_RUN_UNAVAILABLE_MESSAGE: &str = "workflow run is unavailable for this thread";
const WORKFLOW_PAUSE_UNAVAILABLE_MESSAGE: &str = "workflow run is unavailable for pause";
const WORKFLOW_RESUME_UNAVAILABLE_MESSAGE: &str = "workflow run is unavailable for resume";
const WORKFLOW_AGENT_CONTROL_UNAVAILABLE_MESSAGE: &str =
    "workflow agent is unavailable for control";

#[derive(Clone)]
pub(crate) struct WorkflowRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    workflows_service: Arc<WorkflowsService>,
    workflow_feature_enabled: bool,
    codex_home: PathBuf,
    state_db: Option<StateDbHandle>,
}

impl WorkflowRequestProcessor {
    pub(crate) fn new(
        thread_manager: Arc<ThreadManager>,
        workflows_service: Arc<WorkflowsService>,
        workflow_feature_enabled: bool,
        codex_home: PathBuf,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        Self {
            thread_manager,
            workflows_service,
            workflow_feature_enabled,
            codex_home,
            state_db,
        }
    }

    pub(crate) async fn workflow_list(
        &self,
        params: WorkflowListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowListParams {
            thread_id,
            cursor,
            limit,
        } = params;
        let (_thread, registry) = self.thread_registry(&thread_id).await?;
        let total = registry.workflows().len();
        let start = match cursor {
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))?,
            None => 0,
        };
        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total workflows {total}"
            )));
        }

        let page_size = (limit.unwrap_or(WORKFLOW_LIST_DEFAULT_LIMIT as u32) as usize)
            .clamp(1, WORKFLOW_LIST_MAX_LIMIT);
        let end = start.saturating_add(page_size).min(total);
        let data = registry.workflows()[start..end]
            .iter()
            .map(|workflow| WorkflowMetadata {
                name: workflow.name.clone(),
                description: workflow.description.clone(),
                phases: workflow.phases.clone(),
                scope: match workflow.scope {
                    CoreWorkflowScope::Project => WorkflowScope::Project,
                    CoreWorkflowScope::Personal => WorkflowScope::Personal,
                    CoreWorkflowScope::CodexHome => WorkflowScope::CodexHome,
                },
                path: LegacyAppPathString::from_path(&workflow.path),
            })
            .collect();
        let next_cursor = (end < total).then(|| end.to_string());

        Ok(Some(WorkflowListResponse { data, next_cursor }.into()))
    }

    pub(crate) async fn workflow_start(
        &self,
        params: WorkflowStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowStartParams {
            thread_id,
            name,
            args,
        } = params;
        validate_workflow_name(&name)?;
        let args = args.unwrap_or(serde_json::Value::Null);
        validate_workflow_args(&args)?;

        let (thread, registry) = self.thread_registry(&thread_id).await?;
        if registry.resolve_by_name(&name).is_none() {
            return Err(invalid_request(format!("saved workflow not found: {name}")));
        }

        let run_id =
            thread
                .start_saved_workflow(&name, args)
                .await
                .map_err(|error| match error {
                    CodexErr::InvalidRequest(message) => invalid_request(message),
                    error => {
                        internal_error(format!("failed to start saved workflow `{name}`: {error}"))
                    }
                })?;
        Ok(Some(WorkflowStartResponse { run_id }.into()))
    }

    pub(crate) async fn workflow_read(
        &self,
        params: WorkflowReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if !self.workflow_feature_enabled {
            return Err(invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE));
        }
        let WorkflowReadParams { thread_id, run_id } = params;
        let thread_id = ThreadId::from_string(&thread_id)
            .map_err(|_| invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE))?
            .to_string();
        let run_id = canonical_workflow_run_id(&run_id, WORKFLOW_RUN_UNAVAILABLE_MESSAGE)?;
        let meta = self.workflow_run_meta(&run_id).await?;
        if meta.owner_thread_id.as_deref() != Some(thread_id.as_str()) {
            return Err(invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE));
        }

        let report = codex_core::workflow_recovery::reconcile_stale_workflow_run(
            &self.codex_home,
            &run_id,
            self.state_db.as_deref(),
        )
        .await;
        for diagnostic in &report.diagnostics {
            warn!(%run_id, "workflow run read reconciliation: {diagnostic}");
        }

        let meta = self.workflow_run_meta(&run_id).await?;
        if meta.owner_thread_id.as_deref() != Some(thread_id.as_str()) {
            return Err(invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE));
        }
        let status = match meta.status {
            JournalWorkflowRunStatus::Running if report.legacy_unknown == 1 => {
                WorkflowRunStatus::Unknown
            }
            JournalWorkflowRunStatus::Running if report.active == 1 => WorkflowRunStatus::Running,
            JournalWorkflowRunStatus::Running => {
                return Err(internal_error("workflow run status is unavailable"));
            }
            JournalWorkflowRunStatus::Completed => WorkflowRunStatus::Completed,
            JournalWorkflowRunStatus::Stopped => WorkflowRunStatus::Stopped,
            JournalWorkflowRunStatus::Paused => WorkflowRunStatus::Paused,
            JournalWorkflowRunStatus::Failed => WorkflowRunStatus::Failed,
        };
        Ok(Some(WorkflowReadResponse { run_id, status }.into()))
    }

    pub(crate) async fn workflow_stop(
        &self,
        params: WorkflowStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowStopParams { thread_id, run_id } = params;
        let run_id = uuid::Uuid::parse_str(&run_id)
            .map(|run_id| run_id.to_string())
            .map_err(|_| invalid_request(WORKFLOW_RUN_NOT_ACTIVE_MESSAGE))?;
        let thread = self.workflow_thread(&thread_id).await?;
        let disposition = match thread.stop_workflow_run(&run_id).await {
            CoreWorkflowStopDisposition::Applied => WorkflowStopDisposition::Applied,
            CoreWorkflowStopDisposition::AlreadyRequested => {
                WorkflowStopDisposition::AlreadyRequested
            }
            CoreWorkflowStopDisposition::NotRunning => {
                return Err(invalid_request(WORKFLOW_RUN_NOT_ACTIVE_MESSAGE));
            }
        };
        Ok(Some(WorkflowStopResponse { disposition }.into()))
    }

    pub(crate) async fn workflow_pause(
        &self,
        params: WorkflowPauseParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowPauseParams { thread_id, run_id } = params;
        let run_id = canonical_workflow_run_id(&run_id, WORKFLOW_PAUSE_UNAVAILABLE_MESSAGE)?;
        let thread = self
            .workflow_control_thread(&thread_id, WORKFLOW_PAUSE_UNAVAILABLE_MESSAGE)
            .await?;
        let disposition = match thread.pause_workflow_run(&run_id).await {
            CoreWorkflowPauseDisposition::Applied => WorkflowPauseDisposition::Applied,
            CoreWorkflowPauseDisposition::AlreadyRequested => {
                WorkflowPauseDisposition::AlreadyRequested
            }
            CoreWorkflowPauseDisposition::NotRunning => {
                return Err(invalid_request(WORKFLOW_PAUSE_UNAVAILABLE_MESSAGE));
            }
        };
        Ok(Some(WorkflowPauseResponse { disposition }.into()))
    }

    pub(crate) async fn workflow_resume(
        &self,
        params: WorkflowResumeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowResumeParams { thread_id, run_id } = params;
        let source_run_id =
            canonical_workflow_run_id(&run_id, WORKFLOW_RESUME_UNAVAILABLE_MESSAGE)?;
        let thread = self
            .workflow_control_thread(&thread_id, WORKFLOW_RESUME_UNAVAILABLE_MESSAGE)
            .await?;
        let run_id = thread
            .resume_workflow_run(&source_run_id)
            .await
            .map_err(|_| invalid_request(WORKFLOW_RESUME_UNAVAILABLE_MESSAGE))?;
        let run_id = canonical_workflow_run_id(&run_id, WORKFLOW_RESUME_UNAVAILABLE_MESSAGE)?;
        Ok(Some(WorkflowResumeResponse { run_id }.into()))
    }

    pub(crate) async fn workflow_agent_control(
        &self,
        params: WorkflowAgentControlParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowAgentControlParams {
            thread_id,
            run_id,
            node_id,
            attempt,
            action,
        } = params;
        let run_id =
            canonical_workflow_run_id(&run_id, WORKFLOW_AGENT_CONTROL_UNAVAILABLE_MESSAGE)?;
        let thread = self
            .workflow_control_thread(&thread_id, WORKFLOW_AGENT_CONTROL_UNAVAILABLE_MESSAGE)
            .await?;
        let action = match action {
            WorkflowAgentControlAction::Skip => CoreWorkflowAgentControlAction::Skip,
            WorkflowAgentControlAction::Retry => CoreWorkflowAgentControlAction::Retry,
        };
        let response = match thread
            .control_workflow_agent(&run_id, node_id, attempt, action)
            .await
        {
            CoreWorkflowAgentControlDisposition::Skipped => WorkflowAgentControlResponse::Skipped,
            CoreWorkflowAgentControlDisposition::RetryScheduled { attempt } => {
                WorkflowAgentControlResponse::RetryScheduled { attempt }
            }
            CoreWorkflowAgentControlDisposition::RetryLimitReached => {
                WorkflowAgentControlResponse::RetryLimitReached
            }
            CoreWorkflowAgentControlDisposition::Unavailable => {
                return Err(invalid_request(WORKFLOW_AGENT_CONTROL_UNAVAILABLE_MESSAGE));
            }
        };
        Ok(Some(response.into()))
    }

    pub(crate) async fn workflow_save(
        &self,
        params: WorkflowSaveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let WorkflowSaveParams {
            thread_id,
            run_id,
            name,
            scope,
            overwrite,
        } = params;
        let thread = self.workflow_thread(&thread_id).await?;
        let scope = match scope {
            WorkflowSaveScope::Project => ThreadWorkflowSaveScope::Project,
            WorkflowSaveScope::Personal => ThreadWorkflowSaveScope::Personal,
        };
        let mode = if overwrite {
            WorkflowSaveMode::Overwrite
        } else {
            WorkflowSaveMode::Create
        };
        let outcome = thread
            .save_workflow_run(&run_id, &name, scope, mode)
            .await
            .map_err(workflow_save_error)?;
        let disposition = match outcome {
            WorkflowSaveOutcome::Created { .. } => WorkflowSaveDisposition::Created,
            WorkflowSaveOutcome::Overwritten { .. } => WorkflowSaveDisposition::Overwritten,
            WorkflowSaveOutcome::Conflict { .. } => WorkflowSaveDisposition::Conflict,
        };
        if disposition != WorkflowSaveDisposition::Conflict {
            self.workflows_service.clear_cache();
        }
        Ok(Some(WorkflowSaveResponse { disposition }.into()))
    }

    async fn thread_registry(
        &self,
        thread_id: &str,
    ) -> Result<(Arc<CodexThread>, Arc<WorkflowRegistry>), JSONRPCErrorError> {
        let thread = self.workflow_thread(thread_id).await?;
        let roots = self.workflow_roots_for_thread(&thread).await;
        let registry = self.workflows_service.registry_for_roots(roots).await;
        Ok((thread, registry))
    }

    async fn workflow_roots_for_thread(
        &self,
        thread: &CodexThread,
    ) -> Vec<codex_core_workflows::WorkflowRoot> {
        let config = thread.config().await;
        let environments = thread.environment_selections().await;
        let project_cwd = environments.first().and_then(|selection| {
            self.thread_manager
                .environment_manager()
                .get_environment(&selection.environment_id)
                .filter(|environment| !environment.is_remote())
                .and_then(|_| selection.cwd.to_abs_path().ok())
        });
        workflow_roots(
            project_cwd.as_deref(),
            dirs::home_dir().as_deref(),
            Some(config.codex_home.as_path()),
        )
    }

    async fn workflow_thread(
        &self,
        thread_id: &str,
    ) -> Result<Arc<CodexThread>, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;
        let config = thread.config().await;
        if !self.workflow_feature_enabled || !config.features.enabled(Feature::Workflow) {
            return Err(invalid_request(format!(
                "workflow feature is disabled for thread {thread_id}"
            )));
        }
        Ok(thread)
    }

    async fn workflow_control_thread(
        &self,
        thread_id: &str,
        unavailable_message: &'static str,
    ) -> Result<Arc<CodexThread>, JSONRPCErrorError> {
        self.workflow_thread(thread_id)
            .await
            .map_err(|_| invalid_request(unavailable_message))
    }

    async fn workflow_run_meta(&self, run_id: &str) -> Result<WorkflowRunMeta, JSONRPCErrorError> {
        let paths = WorkflowRunPaths::new(&self.codex_home, run_id);
        let meta = tokio::task::spawn_blocking(move || paths.read_meta_bounded())
            .await
            .map_err(|error| {
                warn!(%run_id, %error, "workflow run metadata reader failed");
                invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE)
            })?
            .map_err(|error| {
                warn!(%run_id, %error, "workflow run metadata is unavailable");
                invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE)
            })?;
        if meta.run_id != run_id {
            return Err(invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE));
        }
        Ok(meta)
    }
}

fn canonical_workflow_run_id(
    run_id: &str,
    unavailable_message: &'static str,
) -> Result<String, JSONRPCErrorError> {
    let parsed = uuid::Uuid::parse_str(run_id).map_err(|_| invalid_request(unavailable_message))?;
    let canonical = parsed.to_string();
    if canonical != run_id {
        return Err(invalid_request(unavailable_message));
    }
    Ok(canonical)
}

fn workflow_save_error(error: ThreadWorkflowSaveError) -> JSONRPCErrorError {
    match error {
        ThreadWorkflowSaveError::RunUnavailable => {
            invalid_request(WORKFLOW_RUN_UNAVAILABLE_MESSAGE)
        }
        ThreadWorkflowSaveError::DestinationUnavailable => {
            invalid_request("workflow save destination is unavailable")
        }
        ThreadWorkflowSaveError::Save(error) => match error {
            WorkflowSaveError::InvalidName { .. } => invalid_request("workflow name is invalid"),
            WorkflowSaveError::SourceTooLarge { .. } => invalid_request(format!(
                "workflow run script exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"
            )),
            WorkflowSaveError::InvalidSource { .. }
            | WorkflowSaveError::InvalidWorkflowSource { .. }
            | WorkflowSaveError::SourceHashMismatch { .. }
            | WorkflowSaveError::SourceNameMismatch { .. } => {
                invalid_request("workflow run script cannot be saved")
            }
            WorkflowSaveError::InvalidRoot { .. } | WorkflowSaveError::InvalidTarget { .. } => {
                invalid_request("workflow save destination is unsafe")
            }
            WorkflowSaveError::Io { .. } => internal_error("workflow save failed"),
        },
    }
}

fn validate_workflow_name(name: &str) -> Result<(), JSONRPCErrorError> {
    if name.trim().is_empty() {
        return Err(invalid_request("workflow name must not be empty"));
    }
    if name.len() > WORKFLOW_START_MAX_NAME_BYTES {
        return Err(invalid_request(format!(
            "workflow name exceeds the {WORKFLOW_START_MAX_NAME_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn validate_workflow_args(args: &serde_json::Value) -> Result<(), JSONRPCErrorError> {
    let serialized = serde_json::to_vec(args)
        .map_err(|error| invalid_request(format!("failed to serialize workflow args: {error}")))?;
    if serialized.len() > WORKFLOW_START_MAX_ARGS_BYTES {
        return Err(invalid_request(format!(
            "workflow args exceed the {WORKFLOW_START_MAX_ARGS_BYTES}-byte execution cap"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "workflow_processor_tests.rs"]
mod tests;
