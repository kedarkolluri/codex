use codex_utils_absolute_path::AbsolutePathBuf;

/// Host-resolved workflow discovery inputs frozen when a thread starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowExtensionConfig {
    /// Whether workflow discovery is available for this thread.
    pub enabled: bool,
    /// Host-local Codex home containing the lowest-precedence `workflows` root.
    pub codex_home: AbsolutePathBuf,
    /// Host-local user home containing the optional personal `.agents/workflows` root.
    pub user_home: Option<AbsolutePathBuf>,
    /// Host-local working directory used only when the thread selected no environment.
    pub fallback_cwd: AbsolutePathBuf,
    /// Ordered project-boundary marker names resolved from non-project configuration layers.
    pub project_root_markers: Vec<String>,
}
