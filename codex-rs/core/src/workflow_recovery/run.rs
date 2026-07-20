use std::io;
use std::path::Path;

use codex_state::StateRuntime;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;

use crate::tools::code_mode::workflow_progress::durable::DurableRecoveryTerminal;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_progress::durable::terminalize_interrupted;

use super::WorkflowReconcileReport;

pub(super) async fn reconcile_one(
    codex_home: &Path,
    run_id: &str,
    state_db: Option<&StateRuntime>,
    report: &mut WorkflowReconcileReport,
) -> bool {
    let paths = WorkflowRunPaths::new(codex_home, run_id);
    let meta = match read_meta(paths.clone()).await {
        Ok(meta) => meta,
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` metadata is unavailable: {error}"
            ));
            return false;
        }
    };
    if meta.run_id != run_id {
        report.push_diagnostic(format!(
            "workflow run directory `{run_id}` contains metadata for `{}`",
            meta.run_id
        ));
        return false;
    }
    if meta.status != WorkflowRunStatus::Running {
        report.already_terminal = report.already_terminal.saturating_add(1);
        return project_meta(codex_home, state_db, &meta, report).await;
    }

    let lease_paths = paths.clone();
    let lease = match tokio::task::spawn_blocking(move || {
        WorkflowRunLease::try_acquire_existing(&lease_paths)
    })
    .await
    {
        Ok(Ok(Some(WorkflowRunLeaseAcquire::Acquired(lease)))) => lease,
        Ok(Ok(Some(WorkflowRunLeaseAcquire::Held))) => {
            report.active = report.active.saturating_add(1);
            return project_meta(codex_home, state_db, &meta, report).await;
        }
        // Runs written before lease-based ownership have no marker. Absence
        // cannot distinguish a completed legacy run from one still owned by an
        // older process, so recovery must not mutate either case.
        Ok(Ok(None)) => {
            report.legacy_unknown = report.legacy_unknown.saturating_add(1);
            report.legacy_unknown_run_ids.push(run_id.to_string());
            return project_legacy_unknown(codex_home, state_db, &meta, report).await;
        }
        Ok(Err(error)) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` lease is unavailable: {error}"
            ));
            return false;
        }
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` lease check failed: {error}"
            ));
            return false;
        }
    };

    let meta = match read_meta(paths.clone()).await {
        Ok(meta) => meta,
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` metadata changed during recovery: {error}"
            ));
            return false;
        }
    };
    if meta.run_id != run_id {
        report.push_diagnostic(format!(
            "workflow run directory `{run_id}` changed to metadata for `{}` during recovery",
            meta.run_id
        ));
        return false;
    }
    if meta.status != WorkflowRunStatus::Running {
        report.already_terminal = report.already_terminal.saturating_add(1);
        return project_meta(codex_home, state_db, &meta, report).await;
    }

    let terminal_status = match terminalize_interrupted(codex_home, &meta).await {
        Ok(DurableRecoveryTerminal::Existing(status)) => journal_status_for_progress(&status),
        Ok(DurableRecoveryTerminal::Interrupted {
            replaced_corrupt_snapshot,
        }) => {
            if let Some(error) = replaced_corrupt_snapshot {
                report.push_diagnostic(format!(
                    "workflow run `{run_id}` replaced a corrupt progress snapshot during recovery: {error}"
                ));
            }
            WorkflowRunStatus::Failed
        }
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` could not persist interrupted progress: {error}"
            ));
            WorkflowRunStatus::Failed
        }
    };

    let status_paths = paths.clone();
    let updated_meta = match tokio::task::spawn_blocking(move || {
        status_paths.update_status(terminal_status)
    })
    .await
    {
        Ok(Ok(meta)) => meta,
        Ok(Err(error)) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` could not persist terminal metadata: {error}"
            ));
            return false;
        }
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{run_id}` metadata writer failed: {error}"
            ));
            return false;
        }
    };
    report.reconciled = report.reconciled.saturating_add(1);
    report.reconciled_run_ids.push(run_id.to_string());
    let projected = project_meta(codex_home, state_db, &updated_meta, report).await;
    drop(lease);
    projected
}

async fn project_meta(
    codex_home: &Path,
    state_db: Option<&StateRuntime>,
    meta: &WorkflowRunMeta,
    report: &mut WorkflowReconcileReport,
) -> bool {
    if let Some(state_db) = state_db
        && let Err(error) = state_db
            .upsert_workflow_run(&run_index_params(codex_home, meta))
            .await
    {
        report.push_diagnostic(format!(
            "workflow run `{}` discovery projection update failed: {error}",
            meta.run_id
        ));
        return false;
    }
    true
}

async fn project_legacy_unknown(
    codex_home: &Path,
    state_db: Option<&StateRuntime>,
    meta: &WorkflowRunMeta,
    report: &mut WorkflowReconcileReport,
) -> bool {
    let Some(state_db) = state_db else {
        return true;
    };
    match state_db.get_workflow_run(&meta.run_id).await {
        Ok(Some(run)) if run.status.is_final() => return true,
        Ok(_) => {}
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow run `{}` legacy discovery projection is unavailable: {error}",
                meta.run_id
            ));
            return false;
        }
    }
    let mut params = run_index_params(codex_home, meta);
    params.status = codex_state::WorkflowRunStatus::Unknown;
    if let Err(error) = state_db.upsert_workflow_run(&params).await {
        report.push_diagnostic(format!(
            "workflow run `{}` legacy discovery projection update failed: {error}",
            meta.run_id
        ));
        return false;
    }
    true
}

async fn read_meta(paths: WorkflowRunPaths) -> io::Result<WorkflowRunMeta> {
    tokio::task::spawn_blocking(move || paths.read_meta_bounded())
        .await
        .map_err(io::Error::other)?
}

fn journal_status_for_progress(status: &DurableRunStatus) -> WorkflowRunStatus {
    match status {
        DurableRunStatus::Completed(_) => WorkflowRunStatus::Completed,
        DurableRunStatus::Stopped => WorkflowRunStatus::Stopped,
        DurableRunStatus::Paused => WorkflowRunStatus::Paused,
        DurableRunStatus::PendingInit
        | DurableRunStatus::Running
        | DurableRunStatus::Interrupted
        | DurableRunStatus::Errored(_)
        | DurableRunStatus::Shutdown
        | DurableRunStatus::NotFound => WorkflowRunStatus::Failed,
    }
}

pub(super) fn run_index_params(
    codex_home: &Path,
    meta: &WorkflowRunMeta,
) -> codex_state::WorkflowRunUpsertParams {
    let status = match meta.status {
        WorkflowRunStatus::Running => codex_state::WorkflowRunStatus::Running,
        WorkflowRunStatus::Completed => codex_state::WorkflowRunStatus::Completed,
        WorkflowRunStatus::Stopped => codex_state::WorkflowRunStatus::Stopped,
        WorkflowRunStatus::Paused => codex_state::WorkflowRunStatus::Paused,
        WorkflowRunStatus::Failed => codex_state::WorkflowRunStatus::Failed,
    };
    codex_state::WorkflowRunUpsertParams {
        run_id: meta.run_id.clone(),
        name: meta.name.clone(),
        script_hash: meta.script_hash.clone(),
        script_path: WorkflowRunPaths::new(codex_home, &meta.run_id)
            .script()
            .display()
            .to_string(),
        parent_run_id: meta.parent_run_id.clone(),
        resumed_from_run_id: meta.resumed_from_run_id.clone(),
        owner_thread_id: meta.owner_thread_id.clone(),
        status,
        created_at: meta.created_at.clone(),
    }
}
