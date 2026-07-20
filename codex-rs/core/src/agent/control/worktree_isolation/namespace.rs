//! Crash-recoverable ownership and cleanup for workflow worktree namespaces.

use std::fs;
use std::fs::DirBuilder;
use std::fs::OpenOptions;
use std::io;
use std::sync::Mutex;
use std::sync::MutexGuard;

use codex_utils_absolute_path::AbsolutePathBuf;

use super::WorktreeIsolationError;

static ALLOCATION_ROOT_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn allocation_root_lock() -> MutexGuard<'static, ()> {
    ALLOCATION_ROOT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn prepare_allocation_root(
    allocation_root: &AbsolutePathBuf,
    allocation_claim: &AbsolutePathBuf,
    allocation_tombstone: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    match fs::symlink_metadata(allocation_claim) {
        Ok(_) => {
            return prepare_claimed_allocation_root(
                allocation_root,
                allocation_claim,
                allocation_tombstone,
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorktreeIsolationError::InspectAllocationRoot {
                path: allocation_claim.clone(),
                source,
            });
        }
    }

    match fs::symlink_metadata(allocation_root) {
        Ok(_) => {
            return Err(WorktreeIsolationError::UnownedAllocationRoot {
                path: allocation_root.clone(),
                reason: format!("ownership claim {} is missing", allocation_claim.display()),
            });
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorktreeIsolationError::InspectAllocationRoot {
                path: allocation_root.clone(),
                source,
            });
        }
    }

    match fs::symlink_metadata(allocation_tombstone) {
        Ok(_) => {
            return Err(WorktreeIsolationError::UnownedAllocationRoot {
                path: allocation_tombstone.clone(),
                reason: format!("ownership claim {} is missing", allocation_claim.display()),
            });
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorktreeIsolationError::InspectAllocationRoot {
                path: allocation_tombstone.clone(),
                source,
            });
        }
    }

    match create_allocation_claim(allocation_claim) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return prepare_claimed_allocation_root(
                allocation_root,
                allocation_claim,
                allocation_tombstone,
            );
        }
        Err(source) => {
            return Err(WorktreeIsolationError::CreateAllocationRootClaim {
                path: allocation_claim.clone(),
                source,
            });
        }
    }

    if let Err(create) = create_allocation_root(allocation_root) {
        return match fs::remove_file(allocation_claim) {
            Ok(()) => Err(WorktreeIsolationError::CreateAllocationRoot {
                path: allocation_root.clone(),
                source: create,
            }),
            Err(cleanup) => Err(WorktreeIsolationError::CreateAllocationRootClaimCleanup {
                path: allocation_root.clone(),
                create,
                claim_path: allocation_claim.clone(),
                cleanup,
            }),
        };
    }
    validate_owned_allocation_root(allocation_root, allocation_claim)
}

fn prepare_claimed_allocation_root(
    allocation_root: &AbsolutePathBuf,
    allocation_claim: &AbsolutePathBuf,
    allocation_tombstone: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    validate_allocation_claim(allocation_claim)?;
    remove_owned_allocation_tombstone_if_present(allocation_tombstone)?;
    match fs::symlink_metadata(allocation_root) {
        Ok(_) => validate_owned_allocation_root(allocation_root, allocation_claim),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match create_allocation_root(allocation_root) {
                Ok(()) => validate_owned_allocation_root(allocation_root, allocation_claim),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    validate_owned_allocation_root(allocation_root, allocation_claim)
                }
                Err(source) => Err(WorktreeIsolationError::CreateAllocationRoot {
                    path: allocation_root.clone(),
                    source,
                }),
            }
        }
        Err(source) => Err(WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_root.clone(),
            source,
        }),
    }
}

fn create_allocation_root(allocation_root: &AbsolutePathBuf) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(allocation_root)
}

fn create_allocation_claim(allocation_claim: &AbsolutePathBuf) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(allocation_claim).map(drop)
}

fn validate_allocation_claim(
    allocation_claim: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    let metadata = fs::symlink_metadata(allocation_claim).map_err(|source| {
        WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_claim.clone(),
            source,
        }
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != 0 {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_claim.clone(),
            reason: "ownership claim is not an empty non-symlink regular file".to_string(),
        });
    }
    let canonical_claim = AbsolutePathBuf::from_absolute_path(fs::canonicalize(allocation_claim)?)?;
    if canonical_claim != *allocation_claim {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_claim.clone(),
            reason: format!("path resolves to {}", canonical_claim.display()),
        });
    }
    Ok(())
}

fn validate_owned_allocation_root(
    allocation_root: &AbsolutePathBuf,
    allocation_claim: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    validate_allocation_claim(allocation_claim)?;
    let metadata = fs::symlink_metadata(allocation_root).map_err(|source| {
        WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_root.clone(),
            source,
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_root.clone(),
            reason: "path is not a non-symlink directory".to_string(),
        });
    }
    let canonical_root = AbsolutePathBuf::from_absolute_path(fs::canonicalize(allocation_root)?)?;
    if canonical_root != *allocation_root {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_root.clone(),
            reason: format!("path resolves to {}", canonical_root.display()),
        });
    }
    Ok(())
}

fn validate_empty_allocation_tombstone(
    allocation_tombstone: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    let metadata = fs::symlink_metadata(allocation_tombstone).map_err(|source| {
        WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_tombstone.clone(),
            source,
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_tombstone.clone(),
            reason: "cleanup tombstone is not a non-symlink directory".to_string(),
        });
    }
    let canonical_tombstone =
        AbsolutePathBuf::from_absolute_path(fs::canonicalize(allocation_tombstone)?)?;
    if canonical_tombstone != *allocation_tombstone {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_tombstone.clone(),
            reason: format!("path resolves to {}", canonical_tombstone.display()),
        });
    }
    if fs::read_dir(allocation_tombstone)
        .map_err(|source| WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_tombstone.clone(),
            source,
        })?
        .next()
        .transpose()
        .map_err(|source| WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_tombstone.clone(),
            source,
        })?
        .is_some()
    {
        return Err(WorktreeIsolationError::UnownedAllocationRoot {
            path: allocation_tombstone.clone(),
            reason: "cleanup tombstone is not empty".to_string(),
        });
    }
    Ok(())
}

fn remove_owned_allocation_tombstone_if_present(
    allocation_tombstone: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    match fs::symlink_metadata(allocation_tombstone) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_tombstone.clone(),
            source,
        }),
        Ok(_) => {
            validate_empty_allocation_tombstone(allocation_tombstone)?;
            fs::remove_dir(allocation_tombstone).map_err(|source| {
                WorktreeIsolationError::RemoveAllocationRoot {
                    path: allocation_tombstone.clone(),
                    source,
                }
            })
        }
    }
}

pub(super) fn remove_allocation_root_if_empty(
    allocation_root: &AbsolutePathBuf,
    allocation_claim: &AbsolutePathBuf,
    allocation_tombstone: &AbsolutePathBuf,
) -> Result<(), WorktreeIsolationError> {
    match fs::symlink_metadata(allocation_root) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match fs::symlink_metadata(allocation_claim) {
                Ok(_) => validate_allocation_claim(allocation_claim)?,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return match fs::symlink_metadata(allocation_tombstone) {
                        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                        Ok(_) => Err(WorktreeIsolationError::UnownedAllocationRoot {
                            path: allocation_tombstone.clone(),
                            reason: format!(
                                "ownership claim {} is missing",
                                allocation_claim.display()
                            ),
                        }),
                        Err(source) => Err(WorktreeIsolationError::InspectAllocationRoot {
                            path: allocation_tombstone.clone(),
                            source,
                        }),
                    };
                }
                Err(source) => {
                    return Err(WorktreeIsolationError::InspectAllocationRoot {
                        path: allocation_claim.clone(),
                        source,
                    });
                }
            }
            remove_owned_allocation_tombstone_if_present(allocation_tombstone)?;
            return match fs::remove_file(allocation_claim) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(WorktreeIsolationError::RemoveAllocationRootClaim {
                    path: allocation_claim.clone(),
                    source,
                }),
            };
        }
        Err(source) => {
            return Err(WorktreeIsolationError::InspectAllocationRoot {
                path: allocation_root.clone(),
                source,
            });
        }
        Ok(_) => validate_owned_allocation_root(allocation_root, allocation_claim)?,
    }

    let mut entries = fs::read_dir(allocation_root).map_err(|source| {
        WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_root.clone(),
            source,
        }
    })?;
    if let Some(entry) = entries.next() {
        entry.map_err(|source| WorktreeIsolationError::InspectAllocationRoot {
            path: allocation_root.clone(),
            source,
        })?;
        return Ok(());
    }

    remove_owned_allocation_tombstone_if_present(allocation_tombstone)?;
    fs::rename(allocation_root, allocation_tombstone).map_err(|source| {
        WorktreeIsolationError::MoveAllocationRoot {
            path: allocation_root.clone(),
            tombstone: allocation_tombstone.clone(),
            source,
        }
    })?;
    remove_owned_allocation_tombstone_if_present(allocation_tombstone)?;
    match fs::remove_file(allocation_claim) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorktreeIsolationError::RemoveAllocationRootClaim {
            path: allocation_claim.clone(),
            source,
        }),
    }
}
