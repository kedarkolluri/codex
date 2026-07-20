//! Crash-safe admission for a paused checkpoint's single claimed successor.

use std::path::Path;

use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::storage::WorkflowJournalState;
use codex_workflow_journal::storage::WorkflowRunPaths;

use crate::function_tool::FunctionCallError;

pub(super) enum ResumeSuccessorAdmission {
    Start {
        paths: WorkflowRunPaths,
        lease: WorkflowRunLease,
    },
    Existing,
}

/// Admit a caller-fixed successor id without ever overwriting prior artifacts.
///
/// A free lease plus only exact pre-launch artifacts is retryable. A live
/// lease, terminal metadata, body journal record, or launch marker means the
/// successor was already exposed and must be returned idempotently without a
/// second execution.
pub(super) async fn admit_resume_successor(
    codex_home: &Path,
    run_id: &str,
    source: &str,
    meta: &WorkflowRunMeta,
    args: &serde_json::Value,
) -> Result<ResumeSuccessorAdmission, FunctionCallError> {
    const JOIN_ATTEMPTS: usize = 1_200;
    const JOIN_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

    let paths = WorkflowRunPaths::new(codex_home, run_id);
    paths.create_dir().map_err(admission_error)?;
    let mut attempts = 0;
    let lease = loop {
        match WorkflowRunLease::try_acquire(&paths).map_err(admission_error)? {
            WorkflowRunLeaseAcquire::Acquired(lease) => break lease,
            WorkflowRunLeaseAcquire::Held => {
                match paths.validate_resume_successor_artifacts(source, meta, args) {
                    Ok(status) => {
                        let journal_state = paths.journal_state(meta).map_err(admission_error)?;
                        let launched = paths.was_execution_launched().map_err(admission_error)?;
                        if (status != WorkflowRunStatus::Running || launched)
                            && matches!(
                                journal_state,
                                WorkflowJournalState::Missing
                                    | WorkflowJournalState::Empty
                                    | WorkflowJournalState::PartialHeader
                            )
                        {
                            return Err(admission_error(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "exposed workflow successor has no journal",
                            )));
                        }
                        if status != WorkflowRunStatus::Running
                            || journal_state == WorkflowJournalState::HasBody
                            || launched
                        {
                            return Ok(ResumeSuccessorAdmission::Existing);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(admission_error(error)),
                }
            }
        }
        attempts += 1;
        if attempts >= JOIN_ATTEMPTS {
            return Err(admission_error(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "workflow successor admission timed out",
            )));
        }
        tokio::time::sleep(JOIN_DELAY).await;
    };

    paths
        .ensure_resume_successor_artifacts(source, meta, args)
        .map_err(admission_error)?;
    let persisted = paths.read_meta_bounded().map_err(admission_error)?;
    let journal_state = paths.journal_state(meta).map_err(admission_error)?;
    let launched = paths.was_execution_launched().map_err(admission_error)?;
    if (persisted.status != WorkflowRunStatus::Running || launched)
        && matches!(
            journal_state,
            WorkflowJournalState::Missing
                | WorkflowJournalState::Empty
                | WorkflowJournalState::PartialHeader
        )
    {
        return Err(admission_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "exposed workflow successor has no journal",
        )));
    }
    if persisted.status != WorkflowRunStatus::Running
        || journal_state == WorkflowJournalState::HasBody
        || launched
    {
        return Ok(ResumeSuccessorAdmission::Existing);
    }

    Ok(ResumeSuccessorAdmission::Start { paths, lease })
}

fn admission_error(_error: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(
        "workflow checkpoint successor is unavailable or inconsistent".to_string(),
    )
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
