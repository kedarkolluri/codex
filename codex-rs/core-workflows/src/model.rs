use std::path::PathBuf;

/// Precedence-ordered origin of a saved workflow.
///
/// Ordering matters: a workflow discovered in a higher-precedence scope shadows
/// a same-named workflow found in a lower-precedence scope. The precedence order
/// (highest first) is `Project > Personal > CodexHome`, mirroring the roots in
/// spec §9 (`<repo>/.codex/workflows` > `$HOME/.agents/workflows` >
/// `$CODEX_HOME/workflows`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum WorkflowScope {
    /// `<repo>/.codex/workflows` — project-scoped, checked in (recommended default).
    Project,
    /// `$HOME/.agents/workflows` — personal.
    Personal,
    /// `$CODEX_HOME/workflows`.
    CodexHome,
}

impl WorkflowScope {
    /// Stable lowercase identifier for the scope (useful for picker/registry display).
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowScope::Project => "project",
            WorkflowScope::Personal => "personal",
            WorkflowScope::CodexHome => "codex_home",
        }
    }
}

/// A directory that may contain saved workflow scripts, tagged with its scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRoot {
    /// Absolute path to the workflows directory for this scope.
    pub path: PathBuf,
    /// Scope this root maps to (drives precedence during dedupe).
    pub scope: WorkflowScope,
}

impl WorkflowRoot {
    pub fn new(path: impl Into<PathBuf>, scope: WorkflowScope) -> Self {
        Self {
            path: path.into(),
            scope,
        }
    }
}

/// A single discovered workflow's picker/registry entry.
///
/// Built by statically parsing only the leading `export const meta = {...}`
/// literal of a workflow script; the body is NEVER executed during discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowMetadata {
    /// Workflow name (from `meta.name`).
    pub name: String,
    /// Human-readable description (from `meta.description`).
    pub description: String,
    /// Declared phase titles, in declaration order (from `meta.phases`; may be empty).
    pub phases: Vec<String>,
    /// Absolute path to the workflow script file.
    pub path: PathBuf,
    /// Scope this workflow was discovered in.
    pub scope: WorkflowScope,
}

/// A file that could not be turned into a [`WorkflowMetadata`] entry.
///
/// Discovery is fail-open: a bad file is recorded here and skipped rather than
/// aborting the whole discovery pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowLoadError {
    /// Absolute path to the offending script file.
    pub path: PathBuf,
    /// Human-readable reason the file was skipped.
    pub message: String,
}
