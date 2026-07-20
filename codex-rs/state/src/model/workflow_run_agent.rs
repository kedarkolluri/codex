use std::path::PathBuf;

use codex_protocol::ThreadId;

/// One workflow-owned child transcript in the rebuildable `run_agents` index.
///
/// The workflow journal remains authoritative. This row only provides bounded,
/// indexed lookup for monitors and transcript pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunAgent {
    pub run_id: String,
    pub ordinal: u64,
    pub thread_id: ThreadId,
    /// Host-local absolute path of the child's rollout file.
    pub rollout_path: PathBuf,
}

/// Parameters for live upsert or journal-driven projection rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunAgentUpsertParams {
    pub run_id: String,
    pub ordinal: u64,
    pub thread_id: ThreadId,
    /// Host-local absolute path of the child's rollout file.
    pub rollout_path: PathBuf,
}

#[derive(Debug, sqlx::FromRow)]
pub(crate) struct WorkflowRunAgentRow {
    pub(crate) run_id: String,
    pub(crate) ordinal: i64,
    pub(crate) thread_id: String,
    pub(crate) rollout_path: String,
}

impl TryFrom<WorkflowRunAgentRow> for WorkflowRunAgent {
    type Error = anyhow::Error;

    fn try_from(value: WorkflowRunAgentRow) -> Result<Self, Self::Error> {
        let ordinal = u64::try_from(value.ordinal)
            .map_err(|_| anyhow::anyhow!("invalid workflow agent ordinal: {}", value.ordinal))?;
        let thread_id = ThreadId::from_string(&value.thread_id)
            .map_err(|error| anyhow::anyhow!("invalid workflow agent thread id: {error}"))?;
        let rollout_path = PathBuf::from(value.rollout_path);
        if !rollout_path.is_absolute() {
            anyhow::bail!(
                "workflow agent rollout path is not absolute: {}",
                rollout_path.display()
            );
        }
        Ok(Self {
            run_id: value.run_id,
            ordinal,
            thread_id,
            rollout_path,
        })
    }
}
