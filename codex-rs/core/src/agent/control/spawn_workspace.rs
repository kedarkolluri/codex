//! Typed runtime workspace overrides for spawned agents.
//!
//! A child working in an isolated Git worktree must not receive only a cwd override: the runtime
//! workspace roots and the permission profile materialized against those roots must move with it.
//! This module keeps that three-part update together so callers cannot accidentally create a child
//! whose shell cwd is writable only according to stale parent-workspace permissions.

use crate::config::Config;
use crate::config::Permissions;
use codex_config::ConstraintResult;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Runtime workspace state applied to a child immediately before its thread is spawned.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SpawnAgentWorkspace {
    cwd: AbsolutePathBuf,
    workspace_roots: Vec<AbsolutePathBuf>,
    permissions: Permissions,
}

impl SpawnAgentWorkspace {
    /// Build the child workspace for a detached worktree checkout.
    ///
    /// The parent's complete constrained permission state is retained, including a named profile
    /// identity when present. Only its runtime workspace roots are replaced. This keeps the
    /// canonical profile symbolic and lets [`Permissions::effective_permission_profile`]
    /// materialize `:workspace_roots` against only the isolated checkout, while fixed roots
    /// explicitly granted by the parent profile remain intact.
    pub(crate) fn isolated_worktree(
        cwd: AbsolutePathBuf,
        git_dir: AbsolutePathBuf,
        mut permissions: Permissions,
    ) -> ConstraintResult<Self> {
        let workspace_roots = vec![cwd.clone()];
        permissions.set_workspace_roots(workspace_roots.clone());
        // A linked worktree's `.git` pointer resolves outside the checkout. Root-read profiles
        // already permit that directory semantically, but legacy Landlock needs it represented by
        // an exact read entry. This does not make the Git directory writable.
        permissions.add_explicit_runtime_readable_root(git_dir)?;
        Ok(Self {
            cwd,
            workspace_roots,
            permissions,
        })
    }

    /// Apply the cwd, runtime roots, and materialized profile as one spawn-time operation.
    pub(crate) fn apply(&self, config: &mut Config) {
        config.cwd = self.cwd.clone();
        config.workspace_roots = self.workspace_roots.clone();
        config.workspace_roots_explicit = true;
        config.permissions = self.permissions.clone();
    }
}

#[cfg(test)]
#[path = "spawn_workspace_tests.rs"]
mod tests;
