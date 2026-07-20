//! Deterministic, bounded-memory traversal of the flat durable-run directory.

use std::collections::BinaryHeap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use codex_workflow_journal::WorkflowRecoveryCursorGuard;
use codex_workflow_journal::WorkflowRecoveryCursorRead;
use codex_workflow_journal::storage::ensure_private_runs_root;

#[derive(Clone, Copy)]
pub(super) enum CursorStart {
    Continue,
    Reset,
}

#[derive(Default)]
pub(super) struct RunDirectoryScan {
    pub(super) run_ids: Vec<String>,
    pub(super) truncated: bool,
    pub(super) completed_cycle: bool,
    /// An enumeration/type error may have hidden a canonical directory.
    pub(super) uncertain: bool,
    pub(super) diagnostics: Vec<String>,
}

/// Select the next lexicographic batch after a private durable cursor.
///
/// `std::fs::read_dir` has no portable ordering or seekable cursor. To avoid
/// starving a canonical run behind arbitrary junk after the SQLite projection
/// is lost, this fallback necessarily performs O(directory entries) I/O. It
/// retains at most `2 * limit` canonical IDs, bounds diagnostics in the caller,
/// and returns at most `limit` runs for reconciliation.
pub(super) fn scan_run_ids(
    codex_home: &Path,
    root: &Path,
    limit: usize,
    excluded: &HashSet<String>,
    cursor_start: CursorStart,
    cursor_guard: &WorkflowRecoveryCursorGuard,
) -> io::Result<RunDirectoryScan> {
    let hardened_root = ensure_private_runs_root(codex_home)?;
    if hardened_root != root {
        return Err(invalid_data("workflow runs root hierarchy is invalid"));
    }
    if matches!(cursor_start, CursorStart::Reset) {
        cursor_guard.invalidate()?;
    }
    let mut scan = RunDirectoryScan::default();
    let cursor = match cursor_guard.read()? {
        WorkflowRecoveryCursorRead::MissingOrEmpty => None,
        WorkflowRecoveryCursorRead::Value(cursor) => Some(cursor),
        WorkflowRecoveryCursorRead::Complete => {
            cursor_guard.invalidate()?;
            None
        }
        WorkflowRecoveryCursorRead::InvalidContent(invalid) => {
            super::push_bounded(
                &mut scan.diagnostics,
                format!("invalid workflow recovery cursor ({invalid:?}); reset to scan origin"),
            );
            cursor_guard.invalidate()?;
            None
        }
    };

    let mut after_cursor = BinaryHeap::new();
    let mut wrapped = BinaryHeap::new();
    let mut eligible = 0usize;
    let mut eligible_after_cursor = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                scan.uncertain = true;
                super::push_bounded(
                    &mut scan.diagnostics,
                    format!("failed to inspect workflow run directory: {error}"),
                );
                continue;
            }
        };
        let Some(run_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_canonical_run_id(&run_id) || excluded.contains(&run_id) {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                scan.uncertain = true;
                super::push_bounded(
                    &mut scan.diagnostics,
                    format!("workflow run `{run_id}` type is unavailable: {error}"),
                );
                continue;
            }
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            super::push_bounded(
                &mut scan.diagnostics,
                format!("workflow run `{run_id}` is not a regular, non-symlink directory"),
            );
            continue;
        }

        eligible = eligible.saturating_add(1);
        if cursor.as_ref().is_none_or(|cursor| run_id > *cursor) {
            eligible_after_cursor = eligible_after_cursor.saturating_add(1);
            retain_smallest(&mut after_cursor, run_id, limit);
        } else {
            retain_smallest(&mut wrapped, run_id, limit);
        }
    }

    let mut after_cursor = after_cursor.into_vec();
    after_cursor.sort();
    let remaining = limit.saturating_sub(after_cursor.len());
    let mut wrapped = wrapped.into_vec();
    wrapped.sort();
    wrapped.truncate(remaining);
    scan.run_ids = after_cursor;
    scan.run_ids.extend(wrapped);
    scan.truncated = eligible > scan.run_ids.len();
    scan.completed_cycle = match (&cursor, cursor_start) {
        (_, CursorStart::Reset) | (None, CursorStart::Continue) => eligible <= limit,
        (Some(_), CursorStart::Continue) => eligible_after_cursor <= limit,
    };
    if let Some(next_cursor) = scan.run_ids.last() {
        cursor_guard.replace(next_cursor)?;
    }
    Ok(scan)
}

pub(super) fn is_canonical_run_id(run_id: &str) -> bool {
    uuid::Uuid::parse_str(run_id).is_ok_and(|parsed| parsed.to_string() == run_id)
}

fn retain_smallest(heap: &mut BinaryHeap<String>, run_id: String, limit: usize) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(run_id);
    } else if heap.peek().is_some_and(|largest| run_id < *largest) {
        heap.pop();
        heap.push(run_id);
    }
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
