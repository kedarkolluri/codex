use anyhow::Result;

/// Lifecycle status of a workflow run in the discovery index.
///
/// A run is inserted `running` on start and transitions to `completed` or
/// `failed` on finish. This mirrors the `WorkflowRunEnd{status}` app-server
/// event (spec §9); the table itself is only a discovery projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunStatus {
    Running,
    Completed,
    Failed,
}

impl WorkflowRunStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkflowRunStatus::Running => "running",
            WorkflowRunStatus::Completed => "completed",
            WorkflowRunStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(anyhow::anyhow!("invalid workflow run status: {value}")),
        }
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            WorkflowRunStatus::Completed | WorkflowRunStatus::Failed
        )
    }
}

/// A row of the `workflow_runs` discovery index.
///
/// Stores `{runId, name, scriptHash, scriptPath, parentRunId, status,
/// created_at}` purely for discovery-by-name. Replay never consults this;
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
    pub status: WorkflowRunStatus,
    pub created_at: String,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct WorkflowRunRow {
    pub(crate) run_id: String,
    pub(crate) name: String,
    pub(crate) script_hash: String,
    pub(crate) script_path: String,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) status: String,
    pub(crate) created_at: String,
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
            status: WorkflowRunStatus::parse(value.status.as_str())?,
            created_at: value.created_at,
        })
    }
}
