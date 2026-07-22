use codex_utils_absolute_path::AbsolutePathBuf;

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
    pub path: AbsolutePathBuf,
    pub scope: WorkflowScope,
}

impl WorkflowRoot {
    pub fn new(path: AbsolutePathBuf, scope: WorkflowScope) -> Self {
        Self { path, scope }
    }
}

/// Static metadata for one discovered saved-workflow script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowMetadata {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    /// Absolute host-local path to the script.
    pub path: AbsolutePathBuf,
    pub scope: WorkflowScope,
}

/// A candidate that discovery skipped without aborting the rest of the scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowLoadError {
    pub path: AbsolutePathBuf,
    pub message: String,
}
