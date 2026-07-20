use std::io;
use std::path::Path;

use codex_workflow_journal::WorkflowRecoveryCursor;
use codex_workflow_journal::WorkflowRecoveryCursorGuard;
use codex_workflow_journal::WorkflowRecoveryCursorRead;

pub(super) enum RecoveryCursorState {
    Origin,
    Position,
    Complete,
}

pub(super) struct LockedRecoveryCursor {
    pub(super) guard: WorkflowRecoveryCursorGuard,
    pub(super) state: RecoveryCursorState,
    pub(super) diagnostic: Option<String>,
}

pub(super) fn try_lock_recovery_cursor(
    codex_home: &Path,
) -> io::Result<Option<LockedRecoveryCursor>> {
    let cursor = WorkflowRecoveryCursor::open(codex_home)?;
    let Some(guard) = cursor.try_lock()? else {
        return Ok(None);
    };
    let (state, diagnostic) = match guard.read()? {
        WorkflowRecoveryCursorRead::MissingOrEmpty => (RecoveryCursorState::Origin, None),
        WorkflowRecoveryCursorRead::Value(_) => (RecoveryCursorState::Position, None),
        WorkflowRecoveryCursorRead::Complete => (RecoveryCursorState::Complete, None),
        WorkflowRecoveryCursorRead::InvalidContent(invalid) => {
            guard.invalidate()?;
            (
                RecoveryCursorState::Origin,
                Some(format!(
                    "invalid workflow recovery cursor ({invalid:?}); reset to scan origin"
                )),
            )
        }
    };
    Ok(Some(LockedRecoveryCursor {
        guard,
        state,
        diagnostic,
    }))
}
