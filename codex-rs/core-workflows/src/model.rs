use std::fmt;
use std::sync::Arc;

use codex_file_system::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

/// Origin of a saved workflow, ordered by explicit registry precedence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkflowScope {
    /// A workflow under the current project's `.codex/workflows` directory.
    Project,
    /// A workflow under the user's `.agents/workflows` directory.
    Personal,
    /// A workflow under the configured Codex-home `workflows` directory.
    CodexHome,
}

impl WorkflowScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Personal => "personal",
            Self::CodexHome => "codex_home",
        }
    }

    pub(crate) fn precedence_rank(self) -> u8 {
        match self {
            Self::Project => 0,
            Self::Personal => 1,
            Self::CodexHome => 2,
        }
    }
}

/// An absolute local or executor-backed directory from which saved workflows can be discovered.
#[derive(Clone)]
pub struct WorkflowRoot {
    pub path: PathUri,
    pub scope: WorkflowScope,
    pub(crate) discovery: WorkflowRootDiscovery,
}

impl WorkflowRoot {
    pub fn new(path: AbsolutePathBuf, scope: WorkflowScope) -> Self {
        Self {
            path: PathUri::from_abs_path(&path),
            scope,
            discovery: WorkflowRootDiscovery::HostLocal(path),
        }
    }

    /// Builds a project root that must be read through the selected executor filesystem.
    pub fn project_on_executor(
        path: PathUri,
        file_system: Arc<dyn ExecutorFileSystem>,
    ) -> Self {
        Self {
            path,
            scope: WorkflowScope::Project,
            discovery: WorkflowRootDiscovery::Executor(file_system),
        }
    }

    pub(crate) fn host_path(&self) -> Option<&AbsolutePathBuf> {
        match &self.discovery {
            WorkflowRootDiscovery::HostLocal(path) => Some(path),
            WorkflowRootDiscovery::Executor(_) => None,
        }
    }

    pub(crate) fn executor_file_system(&self) -> Option<&Arc<dyn ExecutorFileSystem>> {
        match &self.discovery {
            WorkflowRootDiscovery::HostLocal(_) => None,
            WorkflowRootDiscovery::Executor(file_system) => Some(file_system),
        }
    }
}

impl fmt::Debug for WorkflowRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRoot")
            .field("path", &self.path)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl PartialEq for WorkflowRoot {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.scope == other.scope
            && match (&self.discovery, &other.discovery) {
                (
                    WorkflowRootDiscovery::HostLocal(left),
                    WorkflowRootDiscovery::HostLocal(right),
                ) => left == right,
                (
                    WorkflowRootDiscovery::Executor(left),
                    WorkflowRootDiscovery::Executor(right),
                ) => Arc::ptr_eq(left, right),
                (WorkflowRootDiscovery::HostLocal(_), WorkflowRootDiscovery::Executor(_))
                | (WorkflowRootDiscovery::Executor(_), WorkflowRootDiscovery::HostLocal(_)) => {
                    false
                }
            }
    }
}

impl Eq for WorkflowRoot {}

#[derive(Clone)]
pub(crate) enum WorkflowRootDiscovery {
    HostLocal(AbsolutePathBuf),
    Executor(Arc<dyn ExecutorFileSystem>),
}

/// Static metadata for one discovered saved-workflow script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowMetadata {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    /// Absolute local or foreign-executor URI for the script.
    pub path: PathUri,
    pub scope: WorkflowScope,
}

/// A candidate that discovery skipped without aborting the rest of the scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowLoadError {
    pub path: PathUri,
    pub message: String,
}
