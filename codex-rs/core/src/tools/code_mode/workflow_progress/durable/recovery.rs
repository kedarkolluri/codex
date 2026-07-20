use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::storage::WorkflowRunPaths;

use super::DurableProgressRead;
use super::DurableProgressSnapshot;
use super::DurableRunState;
use super::DurableRunStatus;
use super::MAX_PROGRESS_FILE_BYTES;
use super::MAX_PROGRESS_TEXT_BYTES;
use super::PROGRESS_SCHEMA_VERSION;
use super::lock_progress_write;
use super::read_blocking;
use super::reject_non_regular_target;
use super::validate_root_and_run_id;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DurableRecoveryTerminal {
    Existing(DurableRunStatus),
    Interrupted {
        replaced_corrupt_snapshot: Option<String>,
    },
}

/// Terminalize a run whose process lease was recovered after its former owner exited.
///
/// A valid snapshot retains all observed topology and counters; only the run itself becomes
/// interrupted. Missing or malformed regular snapshots are replaced by a minimal terminal
/// projection derived from bounded run metadata, without synthesizing agent outcomes.
pub(crate) async fn terminalize_interrupted(
    codex_home: &Path,
    meta: &WorkflowRunMeta,
) -> io::Result<DurableRecoveryTerminal> {
    validate_root_and_run_id(codex_home, &meta.run_id)?;
    let paths = WorkflowRunPaths::new(codex_home, &meta.run_id);
    let meta = meta.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_progress_write();
        terminalize_blocking(&paths, &meta)
    })
    .await
    .map_err(io::Error::other)?
}

fn terminalize_blocking(
    paths: &WorkflowRunPaths,
    meta: &WorkflowRunMeta,
) -> io::Result<DurableRecoveryTerminal> {
    let path = paths.progress();
    let (mut snapshot, replaced_corrupt_snapshot) = match read_blocking(paths, &meta.run_id)? {
        DurableProgressRead::Snapshot(snapshot) if snapshot.state == DurableRunState::Terminal => {
            return Ok(DurableRecoveryTerminal::Existing(snapshot.status));
        }
        DurableProgressRead::Snapshot(snapshot) => (snapshot, None),
        DurableProgressRead::Missing => (recovery_seed(meta), None),
        DurableProgressRead::Corrupt(error) => (recovery_seed(meta), Some(error)),
    };
    let spent = snapshot.budget.map_or(0, |budget| budget.spent.max(0));
    let total = meta
        .budget_total
        .map(|total| i64::try_from(total).unwrap_or(i64::MAX));
    snapshot.reduce(&WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: meta.run_id.clone(),
        status: AgentStatus::Interrupted,
        terminal_reason: Some(WorkflowRunTerminalReason::Interrupted),
        spent,
        total,
    }))?;
    snapshot.validate(&meta.run_id)?;
    let json = serde_json::to_string(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if json.len() as u64 > MAX_PROGRESS_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("workflow progress exceeds the {MAX_PROGRESS_FILE_BYTES}-byte cap"),
        ));
    }
    reject_non_regular_target(&path)?;
    paths.write_progress_atomically(&json)?;
    Ok(DurableRecoveryTerminal::Interrupted {
        replaced_corrupt_snapshot,
    })
}

fn recovery_seed(meta: &WorkflowRunMeta) -> DurableProgressSnapshot {
    DurableProgressSnapshot {
        schema_version: PROGRESS_SCHEMA_VERSION,
        sequence: 0,
        run_id: meta.run_id.clone(),
        resumed_from_run_id: meta.resumed_from_run_id.clone(),
        name: bound_text(&meta.name),
        args_digest: bound_text(&meta.args_hash),
        state: DurableRunState::Running,
        status: DurableRunStatus::Running,
        phases: BTreeMap::new(),
        topology: BTreeMap::new(),
        budget: None,
        active_phase_index: None,
        begun: true,
    }
}

fn bound_text(text: &str) -> String {
    if text.len() <= MAX_PROGRESS_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_PROGRESS_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
