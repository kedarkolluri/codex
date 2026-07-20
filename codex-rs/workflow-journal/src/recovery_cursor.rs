//! Private, locked cursor for bounded workflow-run directory recovery.

use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::private_fs::PrivateDirectoryOpenMode;
use crate::private_fs::PrivateFileOpenMode;

const RECOVERY_SUBDIR: &str = "workflow-recovery";
const CURSOR_FILE: &str = "run-directory.cursor";
const LOCK_FILE: &str = "run-directory.lock";
const COMPLETE_CURSOR: &[u8] = b"complete";
const MAX_CURSOR_BYTES: u64 = 36;

/// A private recovery cursor store scoped to one Codex home.
#[derive(Debug)]
pub struct WorkflowRecoveryCursor {
    cursor_path: PathBuf,
    lock_path: PathBuf,
}

/// Exclusive cross-process ownership of recovery and run publication.
///
/// Publishers take this same lock and invalidate the cursor before making a
/// new run visible. Recovery may therefore trust [`WorkflowRecoveryCursorRead::Complete`]
/// only while it owns this guard.
pub struct WorkflowRecoveryCursorGuard {
    _lock: File,
    cursor_path: PathBuf,
}

/// Bounded, authenticated cursor content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowRecoveryCursorRead {
    /// The cursor file is absent or intentionally reset to empty.
    MissingOrEmpty,
    /// Recovery completed a clean directory cycle after the last publication.
    Complete,
    /// A canonical UUID cursor.
    Value(String),
    /// Unsafe filesystem state is reported as `io::Error`; only bounded content
    /// damage reaches this variant and may be reset by recovery.
    InvalidContent(WorkflowRecoveryCursorInvalidContent),
}

/// Non-authoritative cursor content that may be reset without trusting it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowRecoveryCursorInvalidContent {
    Oversized,
    InvalidUtf8,
    NonCanonicalRunId,
}

impl WorkflowRecoveryCursor {
    /// Create or harden the private recovery directory for `codex_home`.
    pub fn open(codex_home: &Path) -> io::Result<Self> {
        if !codex_home.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow recovery requires an absolute Codex home",
            ));
        }
        let root = codex_home.join(RECOVERY_SUBDIR);
        crate::private_fs::prepare_private_directories(
            [root.as_path()],
            PrivateDirectoryOpenMode::CreateIfMissing,
        )?;
        Ok(Self {
            cursor_path: root.join(CURSOR_FILE),
            lock_path: root.join(LOCK_FILE),
        })
    }

    /// Try to own the cursor transaction without waiting.
    pub fn try_lock(&self) -> io::Result<Option<WorkflowRecoveryCursorGuard>> {
        let file = crate::private_fs::open_private_read_write(
            &self.lock_path,
            PrivateFileOpenMode::CreateIfMissing,
            "workflow recovery cursor lock",
        )?;
        match file.try_lock() {
            Ok(()) => Ok(Some(WorkflowRecoveryCursorGuard {
                _lock: file,
                cursor_path: self.cursor_path.clone(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl WorkflowRecoveryCursorGuard {
    /// Read and classify the bounded cursor while retaining the process lock.
    pub fn read(&self) -> io::Result<WorkflowRecoveryCursorRead> {
        let bytes = match crate::private_fs::read_private_bounded(
            &self.cursor_path,
            MAX_CURSOR_BYTES,
            "workflow recovery cursor",
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(WorkflowRecoveryCursorRead::MissingOrEmpty);
            }
            Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                return Ok(WorkflowRecoveryCursorRead::InvalidContent(
                    WorkflowRecoveryCursorInvalidContent::Oversized,
                ));
            }
            Err(error) => return Err(error),
        };
        if bytes.is_empty() {
            return Ok(WorkflowRecoveryCursorRead::MissingOrEmpty);
        }
        if bytes == COMPLETE_CURSOR {
            return Ok(WorkflowRecoveryCursorRead::Complete);
        }
        let cursor = match String::from_utf8(bytes) {
            Ok(cursor) => cursor,
            Err(_) => {
                return Ok(WorkflowRecoveryCursorRead::InvalidContent(
                    WorkflowRecoveryCursorInvalidContent::InvalidUtf8,
                ));
            }
        };
        if !uuid::Uuid::parse_str(&cursor).is_ok_and(|parsed| parsed.to_string() == cursor) {
            return Ok(WorkflowRecoveryCursorRead::InvalidContent(
                WorkflowRecoveryCursorInvalidContent::NonCanonicalRunId,
            ));
        }
        Ok(WorkflowRecoveryCursorRead::Value(cursor))
    }

    /// Durably replace the cursor with one canonical UUID.
    pub fn replace(&self, run_id: &str) -> io::Result<()> {
        if !uuid::Uuid::parse_str(run_id).is_ok_and(|parsed| parsed.to_string() == run_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow recovery cursor must be a canonical UUID",
            ));
        }
        crate::private_fs::write_atomically(&self.cursor_path, run_id.as_bytes())
    }

    /// Mark the current recovery cycle complete with no intervening publication.
    pub fn mark_complete(&self) -> io::Result<()> {
        crate::private_fs::write_atomically(&self.cursor_path, COMPLETE_CURSOR)
    }

    /// Durably invalidate any clean-complete marker before publishing a run.
    pub fn invalidate(&self) -> io::Result<()> {
        crate::private_fs::write_atomically(&self.cursor_path, &[])
    }

    /// Durably reset the cursor to the deterministic scan origin.
    pub fn reset(&self) -> io::Result<()> {
        self.invalidate()
    }
}

#[cfg(test)]
#[path = "recovery_cursor_tests.rs"]
mod tests;
