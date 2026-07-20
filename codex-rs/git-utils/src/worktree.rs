use std::ffi::OsString;
use std::fs;
use std::io;

use codex_utils_absolute_path::AbsolutePathBuf;

use crate::GitToolingError;
use crate::operations::ensure_git_repository;
use crate::operations::resolve_repository_root;
use crate::operations::run_git_for_status;
use crate::operations::run_git_for_stdout_bytes;

/// The result of explicitly closing a [`WorktreeGuard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeCleanupOutcome {
    /// The checkout and its Git worktree registration were removed.
    Removed,
    /// The checkout was retained because it could not be proven unchanged.
    RetainedDirty { diagnostic: String },
    /// This guard had already completed cleanup or released a dirty checkout.
    AlreadyClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeSnapshot {
    head: Vec<u8>,
    status: Vec<u8>,
}

/// Owns a detached Git worktree and removes it on close or drop when it is unchanged.
///
/// Call [`Self::close`] when the caller needs to report why a changed worktree was retained.
/// Dropping the guard performs the same cleanup best-effort and never force-removes the checkout.
#[derive(Debug)]
#[must_use = "dropping this guard immediately removes an unchanged worktree"]
pub struct WorktreeGuard {
    repo_root: AbsolutePathBuf,
    destination: AbsolutePathBuf,
    canonical_destination: AbsolutePathBuf,
    git_dir: AbsolutePathBuf,
    git_link: Vec<u8>,
    baseline: WorktreeSnapshot,
    closed: bool,
}

impl WorktreeGuard {
    /// Returns the explicit checkout path supplied to [`worktree_add`].
    pub fn path(&self) -> &AbsolutePathBuf {
        &self.destination
    }

    /// Returns the canonical checkout path captured immediately after Git created it.
    pub fn canonical_path(&self) -> &AbsolutePathBuf {
        &self.canonical_destination
    }

    /// Returns the resolved per-worktree Git administrative directory.
    pub fn git_dir(&self) -> &AbsolutePathBuf {
        &self.git_dir
    }

    /// Removes an unchanged worktree or releases ownership of a changed one.
    ///
    /// Cleanup is idempotent. Errors leave the guard active so a later call, including `Drop`, can
    /// retry. A retained dirty worktree is a successful terminal outcome and is never retried.
    pub fn close(&mut self) -> Result<WorktreeCleanupOutcome, GitToolingError> {
        if self.closed {
            return Ok(WorktreeCleanupOutcome::AlreadyClosed);
        }

        if let Some(diagnostic) = self.checkout_identity_change()? {
            self.closed = true;
            return Ok(WorktreeCleanupOutcome::RetainedDirty { diagnostic });
        }

        let current = snapshot(&self.destination)?;
        if current != self.baseline {
            self.closed = true;
            return Ok(WorktreeCleanupOutcome::RetainedDirty {
                diagnostic: snapshot_change_diagnostic(&self.baseline, &current),
            });
        }

        run_worktree_command(
            &self.repo_root,
            vec![
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--"),
                self.destination.as_os_str().to_os_string(),
            ],
        )?;
        self.closed = true;
        Ok(WorktreeCleanupOutcome::Removed)
    }

    fn checkout_identity_change(&self) -> Result<Option<String>, GitToolingError> {
        let metadata = match fs::symlink_metadata(&self.destination) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Some(format!(
                    "worktree path {} no longer exists",
                    self.destination.display()
                )));
            }
            Err(err) => return Err(err.into()),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Ok(Some(format!(
                "worktree path {} is no longer the original directory",
                self.destination.display()
            )));
        }

        let canonical_destination =
            AbsolutePathBuf::from_absolute_path(fs::canonicalize(&self.destination)?)?;
        if canonical_destination != self.canonical_destination {
            return Ok(Some(format!(
                "worktree path {} now resolves to {}",
                self.destination.display(),
                canonical_destination.display()
            )));
        }

        let git_link_path = self.destination.join(".git");
        let git_link_metadata = match fs::symlink_metadata(&git_link_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Some(format!(
                    "worktree metadata {} no longer exists",
                    git_link_path.display()
                )));
            }
            Err(err) => return Err(err.into()),
        };
        if !git_link_metadata.is_file() || git_link_metadata.file_type().is_symlink() {
            return Ok(Some(format!(
                "worktree metadata {} is no longer the original file",
                git_link_path.display()
            )));
        }
        let git_link = fs::read(&git_link_path)?;
        if git_link != self.git_link {
            return Ok(Some(format!(
                "worktree metadata {} changed",
                git_link_path.display()
            )));
        }

        Ok(None)
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Creates a detached worktree at an explicit, previously absent local path.
///
/// The repository argument must be its exact worktree root. The destination's parent must already
/// exist, and the destination must be outside (and not an ancestor of) that repository. Hooks and
/// interactive credential prompts are disabled for every Git command this helper runs.
pub fn worktree_add(
    repo_root: &AbsolutePathBuf,
    destination: AbsolutePathBuf,
) -> Result<WorktreeGuard, GitToolingError> {
    let repo_root = validate_repository_root(repo_root)?;
    validate_destination(&repo_root, &destination)?;

    run_worktree_command(
        &repo_root,
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            OsString::from("--"),
            destination.as_os_str().to_os_string(),
        ],
    )?;

    let initialized = (|| {
        let canonical_destination =
            AbsolutePathBuf::from_absolute_path(fs::canonicalize(&destination)?)?;
        let repository = gix::open(destination.as_path()).map_err(|err| {
            GitToolingError::InvalidWorktreeDestination {
                path: destination.to_path_buf(),
                reason: format!("failed to resolve linked Git directory: {err}"),
            }
        })?;
        let git_dir = AbsolutePathBuf::from_absolute_path(fs::canonicalize(repository.git_dir())?)?;
        let git_link = fs::read(destination.join(".git"))?;
        let baseline = snapshot(&destination)?;
        Ok::<_, GitToolingError>((canonical_destination, git_dir, git_link, baseline))
    })();
    let (canonical_destination, git_dir, git_link, baseline) = match initialized {
        Ok(initialized) => initialized,
        Err(err) => {
            // This is deliberately non-forced: if checkout filters or a racing writer changed the
            // fresh worktree, Git refuses removal and the user's files are retained.
            let _ = run_worktree_command(
                &repo_root,
                vec![
                    OsString::from("worktree"),
                    OsString::from("remove"),
                    OsString::from("--"),
                    destination.as_os_str().to_os_string(),
                ],
            );
            return Err(err);
        }
    };

    Ok(WorktreeGuard {
        repo_root,
        destination,
        canonical_destination,
        git_dir,
        git_link,
        baseline,
        closed: false,
    })
}

fn validate_repository_root(
    repo_root: &AbsolutePathBuf,
) -> Result<AbsolutePathBuf, GitToolingError> {
    ensure_git_repository(repo_root)?;
    let canonical_root = AbsolutePathBuf::from_absolute_path(fs::canonicalize(repo_root)?)?;
    let discovered_root = resolve_repository_root(&canonical_root)?;
    let discovered_root = AbsolutePathBuf::from_absolute_path(fs::canonicalize(discovered_root)?)?;
    if discovered_root != canonical_root {
        return Err(GitToolingError::InvalidGitRepositoryRoot {
            path: repo_root.to_path_buf(),
            reason: format!(
                "expected the exact repository root {}",
                discovered_root.display()
            ),
        });
    }
    Ok(canonical_root)
}

fn validate_destination(
    repo_root: &AbsolutePathBuf,
    destination: &AbsolutePathBuf,
) -> Result<(), GitToolingError> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(GitToolingError::InvalidWorktreeDestination {
                path: destination.to_path_buf(),
                reason: "path already exists".to_string(),
            });
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let Some(parent) = destination.parent() else {
        return Err(GitToolingError::InvalidWorktreeDestination {
            path: destination.to_path_buf(),
            reason: "path has no parent directory".to_string(),
        });
    };
    let metadata = fs::metadata(&parent)?;
    if !metadata.is_dir() {
        return Err(GitToolingError::InvalidWorktreeDestination {
            path: destination.to_path_buf(),
            reason: "parent is not a directory".to_string(),
        });
    }
    let canonical_parent = AbsolutePathBuf::from_absolute_path(fs::canonicalize(parent)?)?;
    let Some(file_name) = destination.file_name() else {
        return Err(GitToolingError::InvalidWorktreeDestination {
            path: destination.to_path_buf(),
            reason: "path has no final component".to_string(),
        });
    };
    let canonical_destination = canonical_parent.join(file_name);
    if canonical_destination.starts_with(repo_root) || repo_root.starts_with(&canonical_destination)
    {
        return Err(GitToolingError::InvalidWorktreeDestination {
            path: destination.to_path_buf(),
            reason: "path must be outside and must not contain the repository root".to_string(),
        });
    }
    Ok(())
}

fn snapshot(worktree: &AbsolutePathBuf) -> Result<WorktreeSnapshot, GitToolingError> {
    let env = noninteractive_env();
    let head = run_git_for_stdout_bytes(worktree, ["rev-parse", "--verify", "HEAD"], Some(&env))?;
    let status = run_git_for_stdout_bytes(
        worktree,
        [
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        Some(&env),
    )?;
    Ok(WorktreeSnapshot { head, status })
}

fn run_worktree_command(
    repo_root: &AbsolutePathBuf,
    args: Vec<OsString>,
) -> Result<(), GitToolingError> {
    let env = noninteractive_env();
    run_git_for_status(repo_root, args, Some(&env))
}

fn noninteractive_env() -> [(OsString, OsString); 2] {
    [
        (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        (OsString::from("GCM_INTERACTIVE"), OsString::from("Never")),
    ]
}

fn snapshot_change_diagnostic(baseline: &WorktreeSnapshot, current: &WorktreeSnapshot) -> String {
    let mut changes = Vec::new();
    if baseline.head != current.head {
        changes.push(format!(
            "HEAD changed from {} to {}",
            String::from_utf8_lossy(&baseline.head).trim(),
            String::from_utf8_lossy(&current.head).trim()
        ));
    }
    if baseline.status != current.status {
        let status = String::from_utf8_lossy(&current.status).replace('\0', "\n");
        changes.push(if status.is_empty() {
            "worktree status changed from its creation baseline".to_string()
        } else {
            format!(
                "worktree has tracked or untracked changes:\n{}",
                status.trim()
            )
        });
    }
    changes.join("; ")
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
