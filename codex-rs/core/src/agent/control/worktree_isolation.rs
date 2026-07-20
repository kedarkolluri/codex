//! Deterministic Git worktree allocation for workflow `agent()` isolation.
//!
//! Allocation is a pure function of the exact repository containing the effective parent cwd, the
//! durable workflow run id, invocation ordinal, and retry attempt. No clock, random source, or
//! process-global counter participates. Creation remains a separate step so callers can validate
//! options and journal a deterministic error before any child thread is spawned.

use std::io;

use codex_git_utils::GitToolingError;
use codex_git_utils::WorktreeGuard;
use codex_git_utils::get_git_repo_root;
use codex_git_utils::worktree_add;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use thiserror::Error;

use crate::environment_selection::TurnEnvironmentSnapshot;

mod namespace;

use namespace::allocation_root_lock;
use namespace::prepare_allocation_root;
use namespace::remove_allocation_root_if_empty;

const MAX_RUN_ID_BYTES: usize = 128;
const ALLOCATION_CLAIM_SUFFIX: &str = ".claim";
const ALLOCATION_TOMBSTONE_SUFFIX: &str = ".removing";

/// Model- and client-visible reason for every host-derived worktree setup failure.
///
/// The underlying errors intentionally retain paths and Git diagnostics for local tracing, but
/// those details must not cross into workflow output or progress events.
pub(crate) const WORKFLOW_WORKTREE_SETUP_FAILED: &str = "workflow agent worktree setup failed";

/// Validated workflow `agent()` isolation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorkflowAgentIsolation {
    /// No isolation option was supplied; inherit the normal child workspace.
    #[default]
    Inherited,
    /// Run the child from a fresh detached Git worktree.
    Worktree,
}

impl WorkflowAgentIsolation {
    /// Accept only an omitted/null option or the exact case-sensitive string `worktree`.
    pub(crate) fn parse(isolation: Option<&str>) -> Result<Self, WorktreeIsolationError> {
        match isolation {
            None => Ok(Self::Inherited),
            Some("worktree") => Ok(Self::Worktree),
            Some(isolation) => Err(WorktreeIsolationError::UnsupportedIsolation {
                isolation: isolation.to_string(),
            }),
        }
    }
}

/// Proof that a worktree-isolated workflow child can execute on the worktree's host.
///
/// Git creates the checkout on the process host, so a remote executor cannot safely consume its
/// path. Requiring exactly one ready local environment prevents a child from retaining any remote,
/// starting, or additional execution lane that could escape the isolated checkout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorktreeExecutionEnvironment {
    selection: TurnEnvironmentSelection,
    cwd: AbsolutePathBuf,
}

impl WorktreeExecutionEnvironment {
    pub(crate) fn from_snapshot(
        environments: &TurnEnvironmentSnapshot,
    ) -> Result<Self, WorktreeIsolationError> {
        let Some(environment) = environments.single_local_environment() else {
            return Err(WorktreeIsolationError::UnsupportedExecutionEnvironments);
        };
        let cwd = environment
            .cwd()
            .to_abs_path()
            .map_err(|_| WorktreeIsolationError::UnsupportedExecutionEnvironments)?;
        Ok(Self {
            selection: environment.selection(),
            cwd,
        })
    }

    /// Derive the checkout from the selected executor's effective cwd, never stale session config.
    pub(crate) fn allocation(
        &self,
        run_id: &str,
        ordinal: u64,
        attempt: u32,
    ) -> Result<WorktreeAllocation, WorktreeIsolationError> {
        WorktreeAllocation::for_workflow_agent(&self.cwd, run_id, ordinal, attempt)
    }

    /// Preserve the admitted local executor identity while moving execution into the checkout.
    pub(crate) fn selection_at(&self, cwd: &AbsolutePathBuf) -> TurnEnvironmentSelection {
        TurnEnvironmentSelection {
            environment_id: self.selection.environment_id.clone(),
            cwd: PathUri::from_abs_path(cwd),
        }
    }
}

/// A deterministic, not-yet-created checkout allocation for one workflow agent invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeAllocation {
    repo_root: AbsolutePathBuf,
    allocation_root: AbsolutePathBuf,
    allocation_claim: AbsolutePathBuf,
    allocation_tombstone: AbsolutePathBuf,
    destination: AbsolutePathBuf,
}

/// Owns one workflow checkout and its shared run-scoped allocation directory.
///
/// The checkout guard removes only the Git worktree itself. After a clean checkout is removed, this
/// wrapper also removes the allocation directory when it is empty. A concurrent or retained dirty
/// sibling keeps the directory non-empty, so cleanup never removes another agent's artifacts.
#[derive(Debug)]
pub(crate) struct WorkflowWorktreeGuard {
    inner: WorktreeGuard,
    allocation_root: AbsolutePathBuf,
    allocation_claim: AbsolutePathBuf,
    allocation_tombstone: AbsolutePathBuf,
    remove_allocation_root_when_empty: bool,
}

impl WorkflowWorktreeGuard {
    pub(crate) fn path(&self) -> &AbsolutePathBuf {
        self.inner.canonical_path()
    }

    pub(crate) fn git_dir(&self) -> &AbsolutePathBuf {
        self.inner.git_dir()
    }

    pub(crate) fn close(
        &mut self,
    ) -> Result<codex_git_utils::WorktreeCleanupOutcome, WorktreeIsolationError> {
        let _allocation_root_lock = allocation_root_lock();
        let outcome = self.inner.close()?;
        if outcome == codex_git_utils::WorktreeCleanupOutcome::Removed {
            self.remove_allocation_root_when_empty = true;
        }
        if self.remove_allocation_root_when_empty {
            remove_allocation_root_if_empty(
                &self.allocation_root,
                &self.allocation_claim,
                &self.allocation_tombstone,
            )?;
        }
        Ok(outcome)
    }
}

impl Drop for WorkflowWorktreeGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl WorktreeAllocation {
    /// Resolve the exact repository containing `parent_cwd` and derive this invocation's checkout.
    ///
    /// The host-minted workflow run id is restricted to portable filename characters rather than
    /// lossily sanitized, so two distinct durable identities can never collapse onto one path.
    fn for_workflow_agent(
        parent_cwd: &AbsolutePathBuf,
        run_id: &str,
        ordinal: u64,
        attempt: u32,
    ) -> Result<Self, WorktreeIsolationError> {
        validate_run_id(run_id)?;
        let repo_root = get_git_repo_root(parent_cwd.as_path()).ok_or_else(|| {
            WorktreeIsolationError::NonGitWorkingDirectory {
                cwd: parent_cwd.clone(),
            }
        })?;
        let repo_root = AbsolutePathBuf::from_absolute_path_checked(repo_root)?.canonicalize()?;
        let repo_parent = repo_root.parent().ok_or_else(|| {
            WorktreeIsolationError::RepositoryHasNoOutsideParent {
                repo_root: repo_root.clone(),
            }
        })?;

        let mut allocation_root = repo_parent.join(format!(".codex-worktrees-{run_id}"));
        if allocation_root == repo_root {
            allocation_root = repo_parent.join(format!(".codex-worktrees-{run_id}-isolated"));
        }
        let allocation_name = allocation_root
            .as_path()
            .file_name()
            .ok_or_else(|| io::Error::other("workflow allocation root has no file name"))?
            .to_string_lossy();
        let allocation_claim =
            repo_parent.join(format!("{allocation_name}{ALLOCATION_CLAIM_SUFFIX}"));
        let allocation_tombstone =
            repo_parent.join(format!("{allocation_name}{ALLOCATION_TOMBSTONE_SUFFIX}"));
        let destination = if attempt == 0 {
            allocation_root.join(format!("agent-{ordinal}"))
        } else {
            allocation_root.join(format!("agent-{ordinal}-attempt-{attempt}"))
        };
        if destination.starts_with(&repo_root) || repo_root.starts_with(&destination) {
            return Err(WorktreeIsolationError::UnsafeDestination {
                repo_root,
                destination,
            });
        }

        Ok(Self {
            repo_root,
            allocation_root,
            allocation_claim,
            allocation_tombstone,
            destination,
        })
    }

    /// Create the allocation parent and detached checkout before any child thread is spawned.
    ///
    /// `worktree_add` rejects an existing destination without overwriting it. A dirty checkout
    /// retained from an earlier attempt therefore becomes a deterministic collision error rather
    /// than being reused or destroyed.
    pub(crate) fn create(self) -> Result<WorkflowWorktreeGuard, WorktreeIsolationError> {
        let _allocation_root_lock = allocation_root_lock();
        prepare_allocation_root(
            &self.allocation_root,
            &self.allocation_claim,
            &self.allocation_tombstone,
        )?;
        let inner = match worktree_add(&self.repo_root, self.destination) {
            Ok(inner) => inner,
            Err(git) => {
                return match remove_allocation_root_if_empty(
                    &self.allocation_root,
                    &self.allocation_claim,
                    &self.allocation_tombstone,
                ) {
                    Ok(()) => Err(git.into()),
                    Err(cleanup) => Err(WorktreeIsolationError::WorktreeCreateCleanup {
                        git,
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
        };
        Ok(WorkflowWorktreeGuard {
            inner,
            allocation_root: self.allocation_root,
            allocation_claim: self.allocation_claim,
            allocation_tombstone: self.allocation_tombstone,
            remove_allocation_root_when_empty: false,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum WorktreeIsolationError {
    #[error(
        "unsupported workflow agent isolation `{isolation}`; expected exact `worktree` or an omitted option"
    )]
    UnsupportedIsolation { isolation: String },
    #[error("workflow agent worktree isolation requires a durable workflow run id")]
    MissingRunIdentity,
    #[error(
        "isolation: 'worktree' requires exactly one ready local execution environment; remote, starting, absent, or multiple environments are unsupported"
    )]
    UnsupportedExecutionEnvironments,
    #[error("workflow agent worktree isolation requires a Git repository containing {cwd}")]
    NonGitWorkingDirectory { cwd: AbsolutePathBuf },
    #[error("workflow run id must be 1..={MAX_RUN_ID_BYTES} portable ASCII bytes, got `{run_id}`")]
    InvalidRunId { run_id: String },
    #[error("cannot allocate an isolated checkout outside repository root {repo_root}")]
    RepositoryHasNoOutsideParent { repo_root: AbsolutePathBuf },
    #[error(
        "derived worktree destination {destination} is not safely outside repository {repo_root}"
    )]
    UnsafeDestination {
        repo_root: AbsolutePathBuf,
        destination: AbsolutePathBuf,
    },
    #[error("failed to create workflow worktree allocation directory {path}: {source}")]
    CreateAllocationRoot {
        path: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create workflow worktree allocation claim {path}: {source}")]
    CreateAllocationRootClaim {
        path: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "failed to create workflow worktree allocation directory {path}: {create}; additionally failed to remove allocation claim {claim_path}: {cleanup}"
    )]
    CreateAllocationRootClaimCleanup {
        path: AbsolutePathBuf,
        #[source]
        create: io::Error,
        claim_path: AbsolutePathBuf,
        cleanup: io::Error,
    },
    #[error("failed to inspect workflow worktree allocation path {path}: {source}")]
    InspectAllocationRoot {
        path: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error("workflow worktree allocation path {path} is not owned by Codex: {reason}")]
    UnownedAllocationRoot {
        path: AbsolutePathBuf,
        reason: String,
    },
    #[error(
        "failed to move empty workflow worktree allocation directory {path} to tombstone {tombstone}: {source}"
    )]
    MoveAllocationRoot {
        path: AbsolutePathBuf,
        tombstone: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to remove empty workflow worktree allocation directory {path}: {source}")]
    RemoveAllocationRoot {
        path: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to remove workflow worktree allocation claim {path}: {source}")]
    RemoveAllocationRootClaim {
        path: AbsolutePathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "failed to create workflow worktree: {git}; allocation namespace cleanup also failed: {cleanup}"
    )]
    WorktreeCreateCleanup {
        #[source]
        git: GitToolingError,
        cleanup: Box<WorktreeIsolationError>,
    },
    #[error(transparent)]
    Git(#[from] GitToolingError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn validate_run_id(run_id: &str) -> Result<(), WorktreeIsolationError> {
    if run_id.is_empty()
        || run_id.len() > MAX_RUN_ID_BYTES
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(WorktreeIsolationError::InvalidRunId {
            run_id: run_id.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "worktree_isolation_tests.rs"]
mod tests;
