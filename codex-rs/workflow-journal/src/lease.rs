//! Cross-process ownership lease for a live workflow run.

use std::fs;
use std::fs::File;
use std::io;
use std::path::PathBuf;

use crate::storage::WorkflowRunPaths;

/// Result of attempting to acquire a run's exclusive advisory lease.
#[derive(Debug)]
pub enum WorkflowRunLeaseAcquire {
    /// This handle now owns the run until the contained lease is dropped.
    Acquired(WorkflowRunLease),
    /// Another process or handle still owns the run.
    Held,
}

/// An exclusive advisory lock on one run's persistent `lease.lock` file.
///
/// The operating system releases the lock when this value is dropped, including
/// process-crash cleanup. The file itself intentionally remains in place.
#[derive(Debug)]
pub struct WorkflowRunLease {
    _file: File,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum LeaseOpenMode {
    Create,
    Existing,
}

impl WorkflowRunLease {
    /// Try to acquire the lease without waiting.
    ///
    /// The run directory must already exist and must be a real directory rather
    /// than a symlink. A contended lock is reported as [`WorkflowRunLeaseAcquire::Held`].
    pub fn try_acquire(paths: &WorkflowRunPaths) -> io::Result<WorkflowRunLeaseAcquire> {
        Self::try_acquire_inner(paths, LeaseOpenMode::Create)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "workflow run lease disappeared while it was being created",
            )
        })
    }

    /// Try to acquire a lease file that already exists, without creating it.
    ///
    /// `None` means this run predates lease-based ownership (or the marker was
    /// removed concurrently). Recovery must leave such a run untouched because
    /// the absence of a lock cannot prove that its former owner exited.
    pub fn try_acquire_existing(
        paths: &WorkflowRunPaths,
    ) -> io::Result<Option<WorkflowRunLeaseAcquire>> {
        Self::try_acquire_inner(paths, LeaseOpenMode::Existing)
    }

    fn try_acquire_inner(
        paths: &WorkflowRunPaths,
        open_mode: LeaseOpenMode,
    ) -> io::Result<Option<WorkflowRunLeaseAcquire>> {
        let run_metadata = fs::symlink_metadata(paths.run_dir())?;
        if run_metadata.file_type().is_symlink() || !run_metadata.file_type().is_dir() {
            return Err(invalid_data(
                "workflow run lease requires a regular, non-symlink run directory",
            ));
        }

        let path = paths.lease();
        reject_non_regular_target(&path)?;
        let private_mode = match open_mode {
            LeaseOpenMode::Create => crate::private_fs::PrivateFileOpenMode::CreateIfMissing,
            LeaseOpenMode::Existing => crate::private_fs::PrivateFileOpenMode::Existing,
        };
        let file = match crate::private_fs::open_private_read_write(
            &path,
            private_mode,
            "workflow run lease",
        ) {
            Ok(file) => file,
            Err(error)
                if matches!(open_mode, LeaseOpenMode::Existing)
                    && error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let path_metadata = fs::symlink_metadata(&path)?;
        if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
            return Err(invalid_data(
                "workflow run lease target is not a regular, non-symlink file",
            ));
        }
        if !file.metadata()?.is_file() {
            return Err(invalid_data("workflow run lease is not a regular file"));
        }

        match file.try_lock() {
            Ok(()) => Ok(Some(WorkflowRunLeaseAcquire::Acquired(Self {
                _file: file,
                path,
            }))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(Some(WorkflowRunLeaseAcquire::Held)),
            Err(error) => Err(error.into()),
        }
    }

    /// Filesystem path whose open handle carries this lease.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn reject_non_regular_target(path: &std::path::Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(invalid_data(
            "workflow run lease target is not a regular, non-symlink file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod tests;
