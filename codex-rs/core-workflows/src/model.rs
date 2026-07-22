use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRoot {
    path: PathUri,
    pub scope: WorkflowScope,
    source: WorkflowRootSource,
}

impl WorkflowRoot {
    pub fn new(path: AbsolutePathBuf, scope: WorkflowScope) -> Self {
        Self {
            path: PathUri::from_abs_path(&path),
            scope,
            source: WorkflowRootSource::HostLocal(path),
        }
    }

    /// Builds a project root that must be read through the selected executor filesystem.
    pub fn project_on_executor(path: PathUri, file_system: Arc<dyn ExecutorFileSystem>) -> Self {
        Self {
            path,
            scope: WorkflowScope::Project,
            source: WorkflowRootSource::Executor(WorkflowFileSystemAuthority::new(file_system)),
        }
    }

    pub fn path(&self) -> &PathUri {
        &self.path
    }

    pub(crate) fn source(&self) -> &WorkflowRootSource {
        &self.source
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowRootSource {
    HostLocal(AbsolutePathBuf),
    Executor(WorkflowFileSystemAuthority),
}

#[derive(Clone)]
pub(crate) struct WorkflowFileSystemAuthority(Arc<dyn ExecutorFileSystem>);

impl WorkflowFileSystemAuthority {
    fn new(file_system: Arc<dyn ExecutorFileSystem>) -> Self {
        Self(file_system)
    }

    pub(crate) fn file_system(&self) -> &Arc<dyn ExecutorFileSystem> {
        &self.0
    }
}

impl fmt::Debug for WorkflowFileSystemAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExecutorFileSystem(..)")
    }
}

impl PartialEq for WorkflowFileSystemAuthority {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for WorkflowFileSystemAuthority {}

impl Hash for WorkflowFileSystemAuthority {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).cast::<()>().hash(state);
    }
}

/// Static metadata for one discovered saved-workflow script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowMetadata {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    /// Absolute `file:` URI for the script.
    pub path: PathUri,
    pub scope: WorkflowScope,
}

/// A candidate that discovery skipped without aborting the rest of the scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowLoadError {
    pub path: PathUri,
    pub message: String,
}
