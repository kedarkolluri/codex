//! Bounded, crash-safe on-disk projection for detached workflow monitors.
//!
//! `progress.json` is an atomically replaced snapshot, not an event log. A
//! process-global mutex serializes read/reduce/write cycles without retaining
//! one lock per run. It is acquired inside blocking filesystem work so no async
//! task holds the guard across an `.await`. The reducer tolerates host callback
//! completion order: an update, binding, or terminal event may create a
//! placeholder that a later begin event fills in.

mod recovery;
mod reducer;
mod validation;

pub(crate) use recovery::DurableRecoveryTerminal;
pub(crate) use recovery::terminalize_interrupted;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::MutexGuard;

use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::storage::WorkflowRunPaths;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const MAX_PROGRESS_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_PROGRESS_PHASES: usize = 1_024;
pub(crate) const MAX_PROGRESS_NODES: usize = 10_000;
const MAX_PROGRESS_TEXT_BYTES: usize = 4 * 1024;
const PROGRESS_SCHEMA_VERSION: u32 = 1;

static PROGRESS_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurableRunState {
    Running,
    Terminal,
}

/// Durable run-level status stored in `progress.json`.
///
/// The legacy [`AgentStatus`] variants are retained byte-for-byte so existing
/// snapshots remain readable. New run-stop events map the shared wire value
/// `AgentStatus::Shutdown` to the explicit durable `Stopped` variant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurableRunStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed(Option<String>),
    Errored(String),
    Shutdown,
    NotFound,
    Stopped,
    Paused,
}

impl DurableRunStatus {
    fn from_run_end(event: &WorkflowRunEndEvent) -> io::Result<Self> {
        match (&event.status, event.terminal_reason) {
            (status, None) => Ok(Self::from_legacy_status(status)),
            (AgentStatus::Completed(message), Some(WorkflowRunTerminalReason::Completed)) => {
                Ok(Self::Completed(message.clone()))
            }
            (AgentStatus::Errored(error), Some(WorkflowRunTerminalReason::Failed)) => {
                Ok(Self::Errored(error.clone()))
            }
            (AgentStatus::Interrupted, Some(WorkflowRunTerminalReason::Interrupted)) => {
                Ok(Self::Interrupted)
            }
            (AgentStatus::Shutdown, Some(WorkflowRunTerminalReason::Stopped)) => Ok(Self::Stopped),
            (AgentStatus::Interrupted, Some(WorkflowRunTerminalReason::Paused)) => Ok(Self::Paused),
            _ => Err(invalid_data(
                "workflow terminal reason contradicts its coarse status",
            )),
        }
    }

    fn from_legacy_status(status: &AgentStatus) -> Self {
        match status {
            AgentStatus::PendingInit => Self::PendingInit,
            AgentStatus::Running => Self::Running,
            AgentStatus::Interrupted => Self::Interrupted,
            AgentStatus::Completed(message) => Self::Completed(message.clone()),
            AgentStatus::Errored(error) => Self::Errored(error.clone()),
            AgentStatus::Shutdown => Self::Stopped,
            AgentStatus::NotFound => Self::NotFound,
        }
    }
}

fn resolve_run_end_status(
    event: &WorkflowRunEndEvent,
    terminal_status: Option<DurableRunStatus>,
) -> io::Result<DurableRunStatus> {
    let derived = DurableRunStatus::from_run_end(event)?;
    if event.terminal_reason.is_some()
        && terminal_status
            .as_ref()
            .is_some_and(|terminal_status| terminal_status != &derived)
    {
        return Err(invalid_data(
            "workflow terminal projection contradicts its raw terminal reason",
        ));
    }
    Ok(terminal_status.unwrap_or(derived))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurablePhaseState {
    Pending,
    Active,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurableNodeState {
    Active,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DurablePhase {
    pub(crate) index: u64,
    pub(crate) title: String,
    pub(crate) state: DurablePhaseState,
    pub(crate) implicit: bool,
    begun: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DurableGroup {
    pub(crate) id: u64,
    pub(crate) parent_node_id: Option<u64>,
    pub(crate) phase_index: u64,
    pub(crate) kind: WorkflowGroupKind,
    pub(crate) item_count: u64,
    pub(crate) state: DurableNodeState,
    begun: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DurableAgent {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub(crate) parent_node_id: Option<u64>,
    pub(crate) phase_index: u64,
    pub(crate) label: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) child_thread_id: Option<String>,
    pub(crate) state: DurableNodeState,
    pub(crate) status: AgentStatus,
    pub(crate) token_usage: TokenUsage,
    pub(crate) tool_call_count: u64,
    #[serde(default)]
    pub(crate) duration_ms: u64,
    pub(crate) returned_null: bool,
    begun: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub(crate) enum DurableNode {
    Group(DurableGroup),
    Agent(DurableAgent),
}

impl DurableNode {
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::Group(group) => group.id,
            Self::Agent(agent) => agent.id,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DurableBudget {
    pub(crate) spent: i64,
    pub(crate) total: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DurableProgressSnapshot {
    schema_version: u32,
    pub(crate) sequence: u64,
    pub(crate) run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resumed_from_run_id: Option<String>,
    pub(crate) name: String,
    pub(crate) args_digest: String,
    pub(crate) state: DurableRunState,
    pub(crate) status: DurableRunStatus,
    pub(crate) phases: BTreeMap<u64, DurablePhase>,
    pub(crate) topology: BTreeMap<u64, DurableNode>,
    pub(crate) budget: Option<DurableBudget>,
    active_phase_index: Option<u64>,
    begun: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DurableProgressRead {
    Missing,
    Corrupt(String),
    Snapshot(DurableProgressSnapshot),
}

pub(crate) async fn record_event(codex_home: &Path, event: &WorkflowEvent) -> io::Result<()> {
    validate_root_and_run_id(codex_home, event_run_id(event))?;
    let paths = WorkflowRunPaths::new(codex_home, event_run_id(event));
    let event = event.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_progress_write();
        record_event_blocking(&paths, &event, None)
    })
    .await
    .map_err(io::Error::other)?
}

/// Persist a run-end event with a workflow-specific durable status.
///
/// Pause currently shares the broad public `Interrupted` agent status, while
/// this projection retains the controller-level `Paused` distinction needed
/// for safe resume admission.
pub(crate) async fn record_terminal_event(
    codex_home: &Path,
    event: &WorkflowEvent,
    status: DurableRunStatus,
) -> io::Result<()> {
    let WorkflowEvent::RunEnd(_) = event else {
        return Err(invalid_data(
            "workflow terminal projection requires a run-end event",
        ));
    };
    validate_root_and_run_id(codex_home, event_run_id(event))?;
    let paths = WorkflowRunPaths::new(codex_home, event_run_id(event));
    let event = event.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_progress_write();
        record_event_blocking(&paths, &event, Some(status))
    })
    .await
    .map_err(io::Error::other)?
}

fn lock_progress_write() -> MutexGuard<'static, ()> {
    PROGRESS_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) async fn read(codex_home: &Path, run_id: &str) -> io::Result<DurableProgressRead> {
    validate_root_and_run_id(codex_home, run_id)?;
    let paths = WorkflowRunPaths::new(codex_home, run_id);
    let expected_run_id = run_id.to_string();
    tokio::task::spawn_blocking(move || read_blocking(&paths, &expected_run_id))
        .await
        .map_err(io::Error::other)?
}

fn record_event_blocking(
    paths: &WorkflowRunPaths,
    event: &WorkflowEvent,
    terminal_status: Option<DurableRunStatus>,
) -> io::Result<()> {
    let path = paths.progress();
    let expected_run_id = event_run_id(event);
    let mut snapshot = match read_blocking(paths, expected_run_id)? {
        DurableProgressRead::Missing => DurableProgressSnapshot::seed(event),
        DurableProgressRead::Snapshot(snapshot) => snapshot,
        DurableProgressRead::Corrupt(error) => {
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }
    };
    snapshot.reduce_with_terminal_status(event, terminal_status)?;
    snapshot.validate(expected_run_id)?;
    let json = serde_json::to_string(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if json.len() as u64 > MAX_PROGRESS_FILE_BYTES {
        return Err(invalid_data(format!(
            "workflow progress exceeds the {MAX_PROGRESS_FILE_BYTES}-byte cap"
        )));
    }
    reject_non_regular_target(&path)?;
    paths.write_progress_atomically(&json)
}

fn read_blocking(
    paths: &WorkflowRunPaths,
    expected_run_id: &str,
) -> io::Result<DurableProgressRead> {
    if !paths.run_dir().try_exists()? {
        return Ok(DurableProgressRead::Missing);
    }
    let bytes = match paths.read_progress_bounded(/* max_bytes */ MAX_PROGRESS_FILE_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(DurableProgressRead::Missing),
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
            return Ok(DurableProgressRead::Corrupt(format!(
                "workflow progress exceeds the {MAX_PROGRESS_FILE_BYTES}-byte cap"
            )));
        }
        Err(error) => return Err(error),
    };
    let snapshot = match serde_json::from_slice::<DurableProgressSnapshot>(&bytes) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(DurableProgressRead::Corrupt(format!(
                "workflow progress contains invalid JSON: {error}"
            )));
        }
    };
    match snapshot.validate(expected_run_id) {
        Ok(()) => Ok(DurableProgressRead::Snapshot(snapshot)),
        Err(error) => Ok(DurableProgressRead::Corrupt(error.to_string())),
    }
}

fn reject_non_regular_target(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(invalid_data(
            "workflow progress target is not a regular, non-symlink file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_root_and_run_id(codex_home: &Path, run_id: &str) -> io::Result<()> {
    if !codex_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workflow progress requires an absolute Codex home",
        ));
    }
    uuid::Uuid::parse_str(run_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(())
}

fn event_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
