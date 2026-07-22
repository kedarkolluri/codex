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

/// An absolute host-local directory from which saved workflows can be discovered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRoot {
    path: PathUri,
    pub scope: WorkflowScope,
    host_path: AbsolutePathBuf,
}

impl WorkflowRoot {
    pub fn new(path: AbsolutePathBuf, scope: WorkflowScope) -> Self {
        Self {
            path: PathUri::from_abs_path(&path),
            scope,
            host_path: path,
        }
    }

    pub fn path(&self) -> &PathUri {
        &self.path
    }

    pub(crate) fn host_path(&self) -> &AbsolutePathBuf {
        &self.host_path
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
