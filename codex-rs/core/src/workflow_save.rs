//! Thread-authorized adapter for saving one durable workflow run script.

use codex_core_workflows::WorkflowSaveMode;
use codex_core_workflows::WorkflowSaveOutcome;
use codex_core_workflows::WorkflowSaveRoot;
use codex_core_workflows::WorkflowSaveSourceIdentity;
use codex_core_workflows::save_run_workflow;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow_journal::storage::WorkflowRunPaths;

use crate::CodexThread;

/// Failure from a thread-authorized workflow save.
#[derive(Debug, thiserror::Error)]
pub enum ThreadWorkflowSaveError {
    /// The run identifier is malformed, absent, or not owned by this thread.
    #[error("workflow run is unavailable for this thread")]
    RunUnavailable,
    /// The requested server-owned destination is unavailable for this thread.
    #[error("workflow save destination is unavailable")]
    DestinationUnavailable,
    /// The owned run was found, but its script could not be saved safely.
    #[error(transparent)]
    Save(#[from] codex_core_workflows::WorkflowSaveError),
}

/// Server-owned saved-workflow destination for a thread-authorized save.
///
/// There is intentionally no arbitrary-path or Codex-home variant. Project
/// roots come only from a host-local primary thread environment and personal
/// roots come only from the app-server host's home directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadWorkflowSaveScope {
    Project,
    Personal,
}

impl CodexThread {
    /// Authorize a durable workflow run against this exact thread and save only
    /// its bounded `script.js` into a caller-selected server-owned root.
    ///
    /// Canonical UUID validation happens before constructing the run path. The
    /// bounded run metadata must repeat the exact run id and requested workflow
    /// name, carry this thread's owner id, and supply the script hash verified by
    /// the writer. Missing legacy ownership never acquires mutation authority.
    pub async fn save_workflow_run(
        &self,
        run_id: &str,
        workflow_name: &str,
        scope: ThreadWorkflowSaveScope,
        mode: WorkflowSaveMode,
    ) -> Result<WorkflowSaveOutcome, ThreadWorkflowSaveError> {
        if self.is_workflow_managed_agent() {
            return Err(ThreadWorkflowSaveError::RunUnavailable);
        }
        let parsed_run_id =
            uuid::Uuid::parse_str(run_id).map_err(|_| ThreadWorkflowSaveError::RunUnavailable)?;
        if parsed_run_id.to_string() != run_id {
            return Err(ThreadWorkflowSaveError::RunUnavailable);
        }

        let config = self.config().await;
        let paths = WorkflowRunPaths::new(config.codex_home.as_path(), run_id);
        let metadata_paths = paths.clone();
        let meta = tokio::task::spawn_blocking(move || metadata_paths.read_meta_bounded())
            .await
            .map_err(|_| ThreadWorkflowSaveError::RunUnavailable)?
            .map_err(|_| ThreadWorkflowSaveError::RunUnavailable)?;
        let owner_thread_id = self.session_configured().thread_id.to_string();
        if meta.run_id != run_id
            || meta.owner_thread_id.as_deref() != Some(owner_thread_id.as_str())
            || meta.name != workflow_name
        {
            return Err(ThreadWorkflowSaveError::RunUnavailable);
        }

        let project_cwd = match scope {
            ThreadWorkflowSaveScope::Project => self
                .environment_selections()
                .await
                .into_iter()
                .next()
                .filter(|selection| {
                    selection.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID
                })
                .and_then(|selection| selection.cwd.to_abs_path().ok()),
            ThreadWorkflowSaveScope::Personal => None,
        };
        let home_dir = match scope {
            ThreadWorkflowSaveScope::Project => None,
            ThreadWorkflowSaveScope::Personal => dirs::home_dir(),
        };
        // The selected local cwd and process home are the trusted scope
        // boundaries. Canonicalizing those boundaries avoids rejecting
        // platform-level aliases above them (for example macOS `/var`) while
        // the writer still rejects every symlink/reparse component introduced
        // below the boundary in `.codex/.agents/workflows`.
        let project_cwd = match project_cwd {
            Some(path) => tokio::fs::canonicalize(path).await.ok(),
            None => None,
        };
        let home_dir = match home_dir {
            Some(path) => tokio::fs::canonicalize(path).await.ok(),
            None => None,
        };
        let workflow_root = match scope {
            ThreadWorkflowSaveScope::Project => project_cwd
                .and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok())
                .map(WorkflowSaveRoot::project),
            ThreadWorkflowSaveScope::Personal => home_dir
                .and_then(|path| AbsolutePathBuf::from_absolute_path(path).ok())
                .map(WorkflowSaveRoot::personal),
        }
        .ok_or(ThreadWorkflowSaveError::DestinationUnavailable)?;

        let source_identity =
            WorkflowSaveSourceIdentity::new(workflow_name, meta.script_hash.clone());
        save_run_workflow(paths.run_dir(), &workflow_root, &source_identity, mode)
            .await
            .map_err(Into::into)
    }
}
