use std::sync::Arc;

use codex_file_system::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_path_uri::PathUriParseError;

use crate::WorkflowRoot;
use crate::WorkflowScope;

const AGENTS_DIR_NAME: &str = ".agents";
const CODEX_DIR_NAME: &str = ".codex";
const WORKFLOWS_DIR_NAME: &str = "workflows";
const PROJECT_WORKFLOWS_RELATIVE_PATH: &str = ".codex/workflows";

/// Assembles the fixed saved-workflow roots without probing any filesystem.
///
/// The Codex-home root is mandatory. A user-home root and one already-resolved project boundary
/// can be added explicitly. Project callers choose whether the boundary belongs to the host or to
/// an executor; executor paths remain bound to that filesystem authority and are never projected
/// onto the host. [`Self::into_roots`] emits roots in project, personal, then Codex-home order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRootAssembly {
    codex_home: AbsolutePathBuf,
    user_home: Option<AbsolutePathBuf>,
    project: Option<WorkflowRoot>,
}

impl WorkflowRootAssembly {
    /// Starts an assembly with the mandatory `$CODEX_HOME/workflows` root.
    pub fn new(codex_home: AbsolutePathBuf) -> Self {
        Self {
            codex_home,
            user_home: None,
            project: None,
        }
    }

    /// Adds the host-local `$HOME/.agents/workflows` root.
    pub fn with_user_home(mut self, user_home: AbsolutePathBuf) -> Self {
        self.user_home = Some(user_home);
        self
    }

    /// Adds `<project>/.codex/workflows` as a host-local root.
    ///
    /// `project_root` must already be the resolved project boundary, not an arbitrary session
    /// working directory.
    pub fn with_host_project(mut self, project_root: AbsolutePathBuf) -> Self {
        self.project = Some(WorkflowRoot::new(
            project_root.join(CODEX_DIR_NAME).join(WORKFLOWS_DIR_NAME),
            WorkflowScope::Project,
        ));
        self
    }

    /// Adds `<project>/.codex/workflows` on the supplied executor filesystem.
    ///
    /// `project_root` must already be the resolved project boundary. URI joining is lexical and
    /// cross-platform; an opaque URI fails closed instead of falling back to a host path.
    pub fn with_executor_project(
        mut self,
        project_root: &PathUri,
        file_system: Arc<dyn ExecutorFileSystem>,
    ) -> Result<Self, PathUriParseError> {
        let root = project_root.join(PROJECT_WORKFLOWS_RELATIVE_PATH)?;
        self.project = Some(WorkflowRoot::project_on_executor(root, file_system));
        Ok(self)
    }

    /// Returns optimistic roots in fixed precedence order.
    ///
    /// Missing project or user-home inputs are skipped. The directories themselves need not exist;
    /// discovery handles missing roots without failing the remaining scan.
    pub fn into_roots(self) -> Vec<WorkflowRoot> {
        let Self {
            codex_home,
            user_home,
            project,
        } = self;
        project
            .into_iter()
            .chain(user_home.map(|user_home| {
                WorkflowRoot::new(
                    user_home.join(AGENTS_DIR_NAME).join(WORKFLOWS_DIR_NAME),
                    WorkflowScope::Personal,
                )
            }))
            .chain(std::iter::once(WorkflowRoot::new(
                codex_home.join(WORKFLOWS_DIR_NAME),
                WorkflowScope::CodexHome,
            )))
            .collect()
    }
}

#[cfg(test)]
#[path = "root_assembly_tests.rs"]
mod tests;
