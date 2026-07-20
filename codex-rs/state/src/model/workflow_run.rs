use anyhow::Result;

/// Lifecycle status of a workflow run in the discovery index.
///
/// A run is inserted `running` on start and transitions to `completed`,
/// `failed`, `stopped`, or `paused` on finish. Markerless runs from before durable lifecycle tracking
/// are projected as `unknown`: their artifacts cannot prove whether the former
/// process completed or was interrupted. The table itself is only a discovery
/// projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunStatus {
    Running,
    Completed,
    Stopped,
    Paused,
    Failed,
    Unknown,
}

impl WorkflowRunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkflowRunStatus::Running => "running",
            WorkflowRunStatus::Completed => "completed",
            WorkflowRunStatus::Stopped => "stopped",
            WorkflowRunStatus::Paused => "paused",
            WorkflowRunStatus::Failed => "failed",
            WorkflowRunStatus::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "stopped" => Ok(Self::Stopped),
            "paused" => Ok(Self::Paused),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(anyhow::anyhow!("invalid workflow run status: {value}")),
        }
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Stopped
                | WorkflowRunStatus::Paused
                | WorkflowRunStatus::Failed
        )
    }
}

/// A row of the `workflow_runs` discovery index.
///
/// Stores `{runId, name, scriptHash, scriptPath, parentRunId, resumedFromRunId, ownerThreadId,
/// status, created_at}` purely for discovery-by-name. Replay never consults this;
/// it is a rebuildable projection of the `runs/<runId>/meta.json` set.
///
/// `created_at` is the host-supplied ISO-8601 string kept verbatim so a
/// rebuild reproduces byte-identical rows (no timestamp round-tripping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub run_id: String,
    pub name: String,
    pub script_hash: String,
    pub script_path: String,
    pub parent_run_id: Option<String>,
    pub resumed_from_run_id: Option<String>,
    pub owner_thread_id: Option<String>,
    pub status: WorkflowRunStatus,
    pub created_at: String,
}

/// Parameters for inserting/replacing a `workflow_runs` row. Used both for the
/// on-start upsert and for rebuilding the projection from `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunUpsertParams {
    pub run_id: String,
    pub name: String,
    pub script_hash: String,
    pub script_path: String,
    pub parent_run_id: Option<String>,
    pub resumed_from_run_id: Option<String>,
    pub owner_thread_id: Option<String>,
    pub status: WorkflowRunStatus,
    pub created_at: String,
}

/// A bounded, durably rotated batch of running workflow IDs to inspect during
/// startup recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunRecoveryBatch {
    pub run_ids: Vec<String>,
    /// Whether another running row was outside this batch at claim time.
    pub has_more: bool,
}

/// Bounded cleanup of publication reservations left by crashed publishers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunPendingCleanupBatch {
    pub removed_run_ids: Vec<String>,
    pub has_more: bool,
}

/// Result of reserving a workflow-run discovery row before publishing files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunPublicationAdmission {
    InsertedPending,
    ExistingPending(WorkflowRunStatus),
    ExistingCommitted(WorkflowRunStatus),
}

/// Whether startup recovery can trust the SQLite workflow-run projection or
/// must continue one bounded filesystem backfill cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunFilesystemIndexState {
    Complete,
    ResetCursor,
    ContinueCursor,
}

/// Result of finishing one complete filesystem backfill cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunFilesystemCycleResult {
    Complete,
    Restart,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct WorkflowRunRow {
    pub(crate) run_id: String,
    pub(crate) name: String,
    pub(crate) script_hash: String,
    pub(crate) script_path: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) resumed_from_run_id: Option<String>,
    pub(crate) owner_thread_id: Option<String>,
    pub(crate) status: String,
    pub(crate) created_at: String,
    pub(crate) publication_state: String,
}

impl TryFrom<WorkflowRunRow> for WorkflowRun {
    type Error = anyhow::Error;

    fn try_from(value: WorkflowRunRow) -> Result<Self, Self::Error> {
        Ok(Self {
            run_id: value.run_id,
            name: value.name,
            script_hash: value.script_hash,
            script_path: value.script_path,
            parent_run_id: value.parent_run_id,
            resumed_from_run_id: value.resumed_from_run_id,
            owner_thread_id: value.owner_thread_id,
            status: WorkflowRunStatus::parse(value.status.as_str())?,
            created_at: value.created_at,
        })
    }
}
