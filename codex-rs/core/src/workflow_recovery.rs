//! Bounded recovery of workflow runs whose former process owner exited.

use std::collections::HashSet;
use std::path::Path;

use codex_state::StateRuntime;
use codex_workflow_journal::storage::runs_root;

const MAX_RECONCILE_RUNS: usize = 1_024;
const MIN_FILESYSTEM_RECOVERY_RUNS: usize = 64;
const MAX_INDEXED_RECONCILE_RUNS: usize = MAX_RECONCILE_RUNS - MIN_FILESYSTEM_RECOVERY_RUNS;
const MAX_RECONCILE_DIAGNOSTICS: usize = 64;
const MAX_DIAGNOSTIC_BYTES: usize = 1_024;

mod cursor;
mod run;
mod scan;

use cursor::LockedRecoveryCursor;
use cursor::RecoveryCursorState;
use cursor::try_lock_recovery_cursor;
use run::reconcile_one;
#[cfg(test)]
use run::run_index_params;
use scan::CursorStart;
use scan::is_canonical_run_id;
use scan::scan_run_ids;

#[derive(Clone, Copy)]
struct RecoveryLimits {
    total_runs: usize,
    indexed_runs: usize,
}

#[derive(Clone, Copy)]
enum FilesystemRecoveryMode {
    Skip,
    Scan(CursorStart),
}

#[derive(Clone, Copy)]
enum RecoveryCandidateOrigin {
    Indexed,
    Filesystem,
}

struct RecoveryCandidate {
    run_id: String,
    origin: RecoveryCandidateOrigin,
}

/// Bounded summary of one stale-run recovery pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowReconcileReport {
    pub scanned: usize,
    pub active: usize,
    pub reconciled: usize,
    pub already_terminal: usize,
    /// Runs without a lease marker whose lifecycle cannot be inferred safely.
    pub legacy_unknown: usize,
    pub truncated: bool,
    pub reconciled_run_ids: Vec<String>,
    pub legacy_unknown_run_ids: Vec<String>,
    pub diagnostics: Vec<String>,
}

/// Reconcile a bounded, durably rotated batch of canonical UUID runs.
///
/// A held lease is treated as proof that another process still owns the run and
/// is never modified. Filesystem damage is reported and skipped so one bad run
/// cannot prevent recovery of unrelated runs. When the discovery index is
/// available, its running rows are claimed before the filesystem fallback;
/// both paths durably rotate survivors behind uninspected rows on the next
/// startup. Once the filesystem projection is known complete, startup is an
/// O(K) indexed pass. The exceptional incomplete/index-lost fallback streams
/// the complete flat directory to make enumeration-order-independent progress,
/// but retains and reconciles only the bounded batch in memory.
pub async fn reconcile_stale_workflow_runs(
    codex_home: &Path,
    state_db: Option<&StateRuntime>,
) -> WorkflowReconcileReport {
    reconcile_stale_workflow_runs_with_limits(
        codex_home,
        state_db,
        RecoveryLimits {
            total_runs: MAX_RECONCILE_RUNS,
            indexed_runs: MAX_INDEXED_RECONCILE_RUNS,
        },
    )
    .await
}

async fn reconcile_stale_workflow_runs_with_limits(
    codex_home: &Path,
    state_db: Option<&StateRuntime>,
    limits: RecoveryLimits,
) -> WorkflowReconcileReport {
    let mut report = WorkflowReconcileReport::default();
    if !codex_home.is_absolute() {
        report.push_diagnostic("workflow recovery requires an absolute Codex home");
        return report;
    }
    if limits.total_runs == 0 || limits.indexed_runs > limits.total_runs {
        report.push_diagnostic("workflow recovery limits are invalid");
        return report;
    }
    let mut locked_cursor = match try_lock_recovery_cursor(codex_home) {
        Ok(Some(cursor)) => Some(cursor),
        Ok(None) => {
            report.push_diagnostic(
                "workflow publication or filesystem recovery is active in another process",
            );
            None
        }
        Err(error) => {
            report.push_diagnostic(format!(
                "workflow recovery cursor is unavailable; filesystem recovery disabled: {error}"
            ));
            None
        }
    };
    if let Some(diagnostic) = locked_cursor
        .as_mut()
        .and_then(|cursor| cursor.diagnostic.take())
    {
        report.push_diagnostic(diagnostic);
    }

    let mut allow_filesystem_completion = true;
    if let (Some(state_db), Some(cursor)) = (state_db, locked_cursor.as_mut()) {
        match state_db
            .discard_pending_workflow_run_publications(limits.total_runs)
            .await
        {
            Ok(cleanup) => {
                if !cleanup.removed_run_ids.is_empty() {
                    report.push_diagnostic(format!(
                        "workflow recovery discarded {} crashed publication reservation(s)",
                        cleanup.removed_run_ids.len()
                    ));
                    // The SQLite transaction made the index incomplete before
                    // deleting pending rows. Reset the second store only after
                    // that durable commit, so a crash at either boundary
                    // necessarily causes another scan.
                    if let Err(error) = cursor.guard.invalidate() {
                        report.push_diagnostic(format!(
                            "workflow recovery cursor could not reset after pending cleanup: {error}"
                        ));
                        locked_cursor = None;
                    } else {
                        cursor.state = RecoveryCursorState::Origin;
                    }
                }
                if cleanup.has_more {
                    allow_filesystem_completion = false;
                    report.truncated = true;
                }
            }
            Err(error) => {
                allow_filesystem_completion = false;
                report.push_diagnostic(format!(
                    "workflow pending-publication cleanup is unavailable: {error}"
                ));
            }
        }
    }

    let filesystem_mode = match (state_db, locked_cursor.as_mut()) {
        (_, None) => FilesystemRecoveryMode::Skip,
        (None, Some(cursor)) => {
            if matches!(cursor.state, RecoveryCursorState::Complete) {
                if let Err(error) = cursor.guard.invalidate() {
                    report.push_diagnostic(format!(
                        "workflow recovery cursor could not restart without an index: {error}"
                    ));
                    locked_cursor = None;
                    FilesystemRecoveryMode::Skip
                } else {
                    cursor.state = RecoveryCursorState::Origin;
                    FilesystemRecoveryMode::Scan(CursorStart::Reset)
                }
            } else {
                FilesystemRecoveryMode::Scan(CursorStart::Continue)
            }
        }
        (Some(state_db), Some(cursor)) => {
            match state_db.prepare_workflow_run_filesystem_index().await {
                Ok(codex_state::WorkflowRunFilesystemIndexState::Complete)
                    if matches!(cursor.state, RecoveryCursorState::Complete) =>
                {
                    FilesystemRecoveryMode::Skip
                }
                Ok(codex_state::WorkflowRunFilesystemIndexState::Complete) => {
                    // DB incomplete first, then cursor origin. The opposite
                    // order could strand a dirty cursor behind a Complete row.
                    match state_db.reset_workflow_run_filesystem_index().await {
                        Ok(()) => match cursor.guard.invalidate() {
                            Ok(()) => {
                                cursor.state = RecoveryCursorState::Origin;
                                FilesystemRecoveryMode::Scan(CursorStart::Reset)
                            }
                            Err(error) => {
                                report.push_diagnostic(format!(
                                    "workflow recovery cursor could not reset: {error}"
                                ));
                                locked_cursor = None;
                                FilesystemRecoveryMode::Skip
                            }
                        },
                        Err(error) => {
                            allow_filesystem_completion = false;
                            report.push_diagnostic(format!(
                                "workflow recovery index could not be reopened: {error}"
                            ));
                            FilesystemRecoveryMode::Scan(CursorStart::Continue)
                        }
                    }
                }
                Ok(codex_state::WorkflowRunFilesystemIndexState::ResetCursor) => {
                    match cursor.guard.invalidate() {
                        Ok(()) => {
                            cursor.state = RecoveryCursorState::Origin;
                            FilesystemRecoveryMode::Scan(CursorStart::Reset)
                        }
                        Err(error) => {
                            report.push_diagnostic(format!(
                                "workflow recovery cursor could not reset: {error}"
                            ));
                            locked_cursor = None;
                            FilesystemRecoveryMode::Skip
                        }
                    }
                }
                Ok(codex_state::WorkflowRunFilesystemIndexState::ContinueCursor)
                    if matches!(cursor.state, RecoveryCursorState::Complete) =>
                {
                    match state_db.reset_workflow_run_filesystem_index().await {
                        Ok(()) => match cursor.guard.invalidate() {
                            Ok(()) => {
                                cursor.state = RecoveryCursorState::Origin;
                                FilesystemRecoveryMode::Scan(CursorStart::Reset)
                            }
                            Err(error) => {
                                report.push_diagnostic(format!(
                                    "workflow recovery cursor could not reset: {error}"
                                ));
                                locked_cursor = None;
                                FilesystemRecoveryMode::Skip
                            }
                        },
                        Err(error) => {
                            allow_filesystem_completion = false;
                            report.push_diagnostic(format!(
                                "workflow recovery index could not restart: {error}"
                            ));
                            FilesystemRecoveryMode::Skip
                        }
                    }
                }
                Ok(codex_state::WorkflowRunFilesystemIndexState::ContinueCursor) => {
                    FilesystemRecoveryMode::Scan(CursorStart::Continue)
                }
                Err(error) => {
                    allow_filesystem_completion = false;
                    report.push_diagnostic(format!(
                        "workflow recovery index completeness is unavailable: {error}"
                    ));
                    FilesystemRecoveryMode::Scan(CursorStart::Continue)
                }
            }
        }
    };

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let indexed_limit = match filesystem_mode {
        FilesystemRecoveryMode::Skip => limits.total_runs,
        FilesystemRecoveryMode::Scan(_) => limits.indexed_runs,
    };
    if let Some(state_db) = state_db
        && indexed_limit > 0
    {
        match state_db
            .claim_running_workflow_runs_for_recovery(indexed_limit)
            .await
        {
            Ok(batch) => {
                report.truncated |= batch.has_more;
                for run_id in batch.run_ids {
                    if !is_canonical_run_id(&run_id) {
                        report.push_diagnostic(format!(
                            "workflow recovery index contains invalid run id `{run_id}`"
                        ));
                        continue;
                    }
                    if seen.insert(run_id.clone()) {
                        candidates.push(RecoveryCandidate {
                            run_id,
                            origin: RecoveryCandidateOrigin::Indexed,
                        });
                    }
                }
            }
            Err(error) => report.push_diagnostic(format!(
                "workflow recovery index traversal is unavailable: {error}"
            )),
        }
    }

    let remaining = limits.total_runs.saturating_sub(candidates.len());
    let mut completed_filesystem_cycle = false;
    let mut filesystem_unresolved = false;
    if let FilesystemRecoveryMode::Scan(cursor_start) = filesystem_mode
        && remaining > 0
        && let Some(cursor) = locked_cursor.take()
    {
        let codex_home = codex_home.to_path_buf();
        let root = runs_root(&codex_home);
        let excluded = seen;
        let guard = cursor.guard;
        let scan = match tokio::task::spawn_blocking(move || {
            let scan = scan_run_ids(
                &codex_home,
                &root,
                remaining,
                &excluded,
                cursor_start,
                &guard,
            );
            (scan, guard)
        })
        .await
        {
            Ok((Ok(scan), guard)) => {
                locked_cursor = Some(LockedRecoveryCursor {
                    guard,
                    state: RecoveryCursorState::Position,
                    diagnostic: None,
                });
                Some(scan)
            }
            Ok((Err(error), guard)) => {
                locked_cursor = Some(LockedRecoveryCursor {
                    guard,
                    state: RecoveryCursorState::Position,
                    diagnostic: None,
                });
                allow_filesystem_completion = false;
                report.push_diagnostic(format!("failed to scan workflow runs: {error}"));
                None
            }
            Err(error) => {
                report.push_diagnostic(format!("workflow recovery scanner failed: {error}"));
                None
            }
        };
        if let Some(scan) = scan {
            report.truncated |= scan.truncated;
            completed_filesystem_cycle = scan.completed_cycle;
            filesystem_unresolved |= scan.uncertain;
            for diagnostic in scan.diagnostics {
                report.push_diagnostic(diagnostic);
            }
            candidates.extend(scan.run_ids.into_iter().map(|run_id| RecoveryCandidate {
                run_id,
                origin: RecoveryCandidateOrigin::Filesystem,
            }));
        }
    }
    if report.truncated {
        report.push_diagnostic(format!(
            "workflow recovery reached its {}-run batch; indexed and filesystem candidates \
             rotate durably across startups",
            limits.total_runs
        ));
    }
    for candidate in candidates {
        report.scanned = report.scanned.saturating_add(1);
        let resolved = reconcile_one(codex_home, &candidate.run_id, state_db, &mut report).await;
        if matches!(candidate.origin, RecoveryCandidateOrigin::Filesystem) && !resolved {
            filesystem_unresolved = true;
        }
    }
    if matches!(filesystem_mode, FilesystemRecoveryMode::Scan(_))
        && let (Some(state_db), Some(cursor)) = (state_db, locked_cursor.as_ref())
        && allow_filesystem_completion
    {
        let mut can_finish_cycle = true;
        if filesystem_unresolved
            && let Err(error) = state_db
                .note_workflow_run_filesystem_index_unresolved()
                .await
        {
            can_finish_cycle = false;
            report.push_diagnostic(format!(
                "workflow recovery could not persist an unresolved directory: {error}"
            ));
        }
        if completed_filesystem_cycle && can_finish_cycle {
            match state_db.finish_workflow_run_filesystem_index_cycle().await {
                Ok(codex_state::WorkflowRunFilesystemCycleResult::Complete) => {
                    // SQLite commits Complete first. If the second-store marker
                    // fails or the process crashes here, the next startup sees
                    // Complete + non-Complete cursor and reopens the index.
                    if let Err(error) = cursor.guard.mark_complete() {
                        report.push_diagnostic(format!(
                            "workflow recovery cursor could not mark completion: {error}"
                        ));
                    }
                }
                Ok(codex_state::WorkflowRunFilesystemCycleResult::Restart) => {
                    if let Err(error) = cursor.guard.invalidate() {
                        report.push_diagnostic(format!(
                            "workflow recovery cursor could not restart an unresolved cycle: {error}"
                        ));
                    }
                }
                Err(error) => report.push_diagnostic(format!(
                    "workflow recovery could not finish its filesystem index cycle: {error}"
                )),
            }
        }
    }
    report
}

/// Reconcile one canonical UUID run directory without scanning sibling runs.
pub async fn reconcile_stale_workflow_run(
    codex_home: &Path,
    run_id: &str,
    state_db: Option<&StateRuntime>,
) -> WorkflowReconcileReport {
    let mut report = WorkflowReconcileReport::default();
    if !codex_home.is_absolute() {
        report.push_diagnostic("workflow recovery requires an absolute Codex home");
        return report;
    }
    if !is_canonical_run_id(run_id) {
        report.push_diagnostic(format!(
            "workflow recovery rejected invalid run id `{run_id}`"
        ));
        return report;
    }
    report.scanned = 1;
    reconcile_one(codex_home, run_id, state_db, &mut report).await;
    report
}

impl WorkflowReconcileReport {
    fn push_diagnostic(&mut self, diagnostic: impl Into<String>) {
        push_bounded(&mut self.diagnostics, diagnostic.into());
    }
}

fn push_bounded(diagnostics: &mut Vec<String>, diagnostic: String) {
    if diagnostics.len() >= MAX_RECONCILE_DIAGNOSTICS {
        return;
    }
    diagnostics.push(bound_text(&diagnostic, MAX_DIAGNOSTIC_BYTES));
}

fn bound_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
#[path = "workflow_recovery_tests.rs"]
mod tests;
