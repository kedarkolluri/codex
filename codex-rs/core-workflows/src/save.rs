//! Safe materialization of a durable workflow run's exact script as a saved workflow.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_code_mode_protocol::WORKFLOW_NAME_MAX_BYTES;
use codex_code_mode_protocol::ensure_workflow_name;
use codex_code_mode_protocol::parse_workflow_meta;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;

use self::filesystem::canonicalize_local_path;
use self::filesystem::configure_no_follow;
use self::filesystem::is_link_or_reparse_point;
use self::publication::publish_workflow;

mod filesystem;
mod publication;

const RUN_SCRIPT_FILE_NAME: &str = "script.js";
const JAVASCRIPT_FILE_SUFFIX: &str = ".js";
const MAX_PORTABLE_FILE_NAME_BYTES: usize = 255;
const MAX_SAVE_NAME_BYTES: usize = {
    let file_stem_limit = MAX_PORTABLE_FILE_NAME_BYTES - JAVASCRIPT_FILE_SUFFIX.len();
    if WORKFLOW_NAME_MAX_BYTES < file_stem_limit {
        WORKFLOW_NAME_MAX_BYTES
    } else {
        file_stem_limit
    }
};
const PROJECT_WORKFLOW_ROOT_COMPONENTS: &[&str] = &[".codex", "workflows"];
const PERSONAL_WORKFLOW_ROOT_COMPONENTS: &[&str] = &[".agents", "workflows"];

/// Hard cap on a run script copied into a saved-workflow root.
///
/// This intentionally matches the execution-time source cap. The save path reads
/// at most one byte beyond the limit and never materializes an unbounded run
/// artifact.
pub const WORKFLOW_SOURCE_MAX_BYTES: u64 = 1024 * 1024;

/// Whether saving may replace an existing, regular workflow file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowSaveMode {
    /// Create a new saved workflow. An existing regular target is returned as a
    /// typed [`WorkflowSaveOutcome::Conflict`] and is left unchanged.
    Create,
    /// Create the target when absent or explicitly replace an existing regular
    /// file. Symlinks, reparse points, and non-regular targets are never followed.
    Overwrite,
}

/// Result of successfully handling a save request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowSaveOutcome {
    /// A new `<name>.js` file was created.
    Created { path: PathBuf },
    /// An existing regular `<name>.js` file was explicitly overwritten.
    Overwritten { path: PathBuf },
    /// Create-only mode found an existing regular target and changed nothing.
    Conflict { path: PathBuf },
}

/// Durable identity that the exact run script must satisfy before publication.
///
/// The name is checked against the statically parsed source metadata, while the
/// hash is checked against the same bounded byte buffer written to the target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSaveSourceIdentity {
    name: String,
    script_hash: String,
}

impl WorkflowSaveSourceIdentity {
    pub fn new(name: impl Into<String>, script_hash: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            script_hash: script_hash.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowSaveRootKind {
    Project,
    Personal,
}

/// Capability boundary and fixed registry suffix for a workflow save.
///
/// Callers select either the project cwd or the process home directory as the
/// trusted boundary. The writer canonicalizes and opens only that boundary,
/// then traverses the fixed registry suffix component-by-component through
/// held directory capabilities. No caller-supplied destination suffix or
/// Codex-home/Claude scope can be represented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSaveRoot {
    trusted_base: AbsolutePathBuf,
    kind: WorkflowSaveRootKind,
}

impl WorkflowSaveRoot {
    /// Save beneath `<project_root>/.codex/workflows`.
    pub fn project(project_root: AbsolutePathBuf) -> Self {
        Self {
            trusted_base: project_root,
            kind: WorkflowSaveRootKind::Project,
        }
    }

    /// Save beneath `<home_dir>/.agents/workflows`.
    pub fn personal(home_dir: AbsolutePathBuf) -> Self {
        Self {
            trusted_base: home_dir,
            kind: WorkflowSaveRootKind::Personal,
        }
    }

    /// Absolute registry path implied by this trusted boundary and scope.
    pub fn path(&self) -> AbsolutePathBuf {
        self.components()
            .iter()
            .fold(self.trusted_base.clone(), |path, component| {
                path.join(component)
            })
    }

    fn components(&self) -> &'static [&'static str] {
        match self.kind {
            WorkflowSaveRootKind::Project => PROJECT_WORKFLOW_ROOT_COMPONENTS,
            WorkflowSaveRootKind::Personal => PERSONAL_WORKFLOW_ROOT_COMPONENTS,
        }
    }

    fn requires_private_permissions(&self) -> bool {
        matches!(self.kind, WorkflowSaveRootKind::Personal)
    }
}

/// Failure to validate or safely copy a workflow run script.
#[derive(Debug)]
pub enum WorkflowSaveError {
    /// The requested workflow name cannot be represented as one portable file stem.
    InvalidName { name: String, reason: String },
    /// The run directory or its exact `script.js` child is unsafe or invalid.
    InvalidSource { path: PathBuf, reason: String },
    /// The run script exceeded [`WORKFLOW_SOURCE_MAX_BYTES`].
    SourceTooLarge { path: PathBuf, max_bytes: u64 },
    /// The bounded source was not a statically valid workflow script.
    InvalidWorkflowSource { path: PathBuf, reason: String },
    /// The exact persisted source bytes do not match the durable run metadata.
    SourceHashMismatch { expected: String, actual: String },
    /// Saving is exact and does not rewrite metadata, so `meta.name` must match
    /// the requested saved name.
    SourceNameMismatch { expected: String, actual: String },
    /// The caller-supplied workflow root is not a safe directory.
    InvalidRoot { path: PathBuf, reason: String },
    /// The destination is a symlink, reparse point, or non-regular filesystem object.
    InvalidTarget { path: PathBuf, reason: String },
    /// A filesystem operation failed after validation.
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for WorkflowSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, reason } => {
                write!(formatter, "invalid workflow name {name:?}: {reason}")
            }
            Self::InvalidSource { path, reason } => {
                write!(
                    formatter,
                    "invalid workflow run source {}: {reason}",
                    path.display()
                )
            }
            Self::SourceTooLarge { path, max_bytes } => write!(
                formatter,
                "workflow run source {} exceeds the {max_bytes}-byte limit",
                path.display()
            ),
            Self::InvalidWorkflowSource { path, reason } => write!(
                formatter,
                "workflow run source {} is invalid: {reason}",
                path.display()
            ),
            Self::SourceHashMismatch { expected, actual } => write!(
                formatter,
                "workflow run source hash {actual:?} does not match durable hash {expected:?}"
            ),
            Self::SourceNameMismatch { expected, actual } => write!(
                formatter,
                "workflow source name {actual:?} does not match requested name {expected:?}"
            ),
            Self::InvalidRoot { path, reason } => {
                write!(
                    formatter,
                    "invalid workflow root {}: {reason}",
                    path.display()
                )
            }
            Self::InvalidTarget { path, reason } => {
                write!(
                    formatter,
                    "invalid workflow target {}: {reason}",
                    path.display()
                )
            }
            Self::Io {
                action,
                path,
                source,
            } => write!(formatter, "failed to {action} {}: {source}", path.display()),
        }
    }
}

impl Error for WorkflowSaveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidName { .. }
            | Self::InvalidSource { .. }
            | Self::SourceTooLarge { .. }
            | Self::InvalidWorkflowSource { .. }
            | Self::SourceHashMismatch { .. }
            | Self::SourceNameMismatch { .. }
            | Self::InvalidRoot { .. }
            | Self::InvalidTarget { .. } => None,
        }
    }
}

/// Copy one durable run's exact, bounded `script.js` into a Codex workflow root.
///
/// The identity name is validated as a portable single-component file stem and
/// the only possible target is its canonical `<name>.js` child of
/// `workflow_root`. The source is always the exact `script.js` child of
/// `run_directory`; callers cannot use this primitive to copy an arbitrary run
/// artifact. The source bytes are copied unchanged, their BLAKE3 hash must
/// match the durable identity, and the statically parsed `meta.name` must match
/// the identity name.
///
/// The run-directory path is host-local, not a protocol value. The destination
/// is a typed [`WorkflowSaveRoot`] whose only caller-selected path is the
/// trusted project/home boundary; the registry suffix is fixed by the scope.
///
/// On Unix, directories created by this operation are mode `0700` and target
/// files are mode `0600`. Existing Personal registry directories are admitted
/// only when the effective user owns them and group/other users cannot write
/// them; their permissions are never rewritten. Publication is prepared in a
/// bounded, same-directory temporary file: create-only mode atomically links it
/// into place without clobbering, and overwrite atomically replaces the target
/// directory entry. On Windows, newly created Personal directories receive a
/// protected current-user, Local-System, and Administrators DACL in the native
/// creation call. Existing Personal directories are admitted only when the
/// current user owns them and their protected allow ACEs satisfy that policy;
/// their ACLs are never rewritten. Both modes rename
/// the open temporary-file handle relative to the held workflow-directory handle,
/// with replacement enabled only for overwrite. Neither platform follows a final
/// symlink/reparse component, and readers see either the complete old source or
/// the complete new source rather than a truncated intermediate file.
pub async fn save_run_workflow(
    run_directory: &Path,
    workflow_root: &WorkflowSaveRoot,
    source_identity: &WorkflowSaveSourceIdentity,
    mode: WorkflowSaveMode,
) -> Result<WorkflowSaveOutcome, WorkflowSaveError> {
    let file_name = workflow_file_name(&source_identity.name)?;
    let source = read_exact_run_script(run_directory, source_identity).await?;
    let workflow_root = workflow_root.clone();
    let task_target = workflow_root.path().join(&file_name).into_path_buf();
    tokio::task::spawn_blocking(move || publish_workflow(&workflow_root, &file_name, &source, mode))
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "join workflow publication task",
            path: task_target,
            source: io::Error::other(source),
        })?
}

fn workflow_file_name(name: &str) -> Result<String, WorkflowSaveError> {
    if let Err(reason) = ensure_workflow_name(name) {
        return Err(WorkflowSaveError::InvalidName {
            name: name.to_string(),
            reason,
        });
    }
    if name.len() > MAX_SAVE_NAME_BYTES {
        return Err(WorkflowSaveError::InvalidName {
            name: name.to_string(),
            reason: format!("name exceeds the {MAX_SAVE_NAME_BYTES}-byte portable filename limit"),
        });
    }
    let mut chars = name.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(WorkflowSaveError::InvalidName {
            name: name.to_string(),
            reason:
                "expected an ASCII alphanumeric stem containing only letters, digits, '-' or '_'"
                    .to_string(),
        });
    }
    if is_windows_reserved_stem(name) {
        return Err(WorkflowSaveError::InvalidName {
            name: name.to_string(),
            reason: "name is reserved by Windows filesystems".to_string(),
        });
    }
    Ok(format!("{name}{JAVASCRIPT_FILE_SUFFIX}"))
}

fn is_windows_reserved_stem(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

async fn read_exact_run_script(
    run_directory: &Path,
    source_identity: &WorkflowSaveSourceIdentity,
) -> Result<Vec<u8>, WorkflowSaveError> {
    let run_directory = AbsolutePathBuf::relative_to_current_dir(run_directory)
        .map_err(|source| WorkflowSaveError::Io {
            action: "resolve run directory",
            path: run_directory.to_path_buf(),
            source,
        })?
        .into_path_buf();
    let run_metadata = tokio::fs::symlink_metadata(&run_directory)
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "inspect run directory",
            path: run_directory.clone(),
            source,
        })?;
    if is_link_or_reparse_point(&run_metadata) || !run_metadata.is_dir() {
        return Err(WorkflowSaveError::InvalidSource {
            path: run_directory,
            reason: "run directory must be a real directory, not a symlink or reparse point"
                .to_string(),
        });
    }
    let canonical_run_directory =
        canonicalize_local_path(&run_directory, "canonicalize run directory").await?;
    let source_path = canonical_run_directory.join(RUN_SCRIPT_FILE_NAME);
    let source_metadata = tokio::fs::symlink_metadata(&source_path)
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "inspect run script",
            path: source_path.clone(),
            source,
        })?;
    if is_link_or_reparse_point(&source_metadata) || !source_metadata.is_file() {
        return Err(WorkflowSaveError::InvalidSource {
            path: source_path,
            reason: "script.js must be a regular file, not a symlink or reparse point".to_string(),
        });
    }
    if source_metadata.len() > WORKFLOW_SOURCE_MAX_BYTES {
        return Err(WorkflowSaveError::SourceTooLarge {
            path: source_path,
            max_bytes: WORKFLOW_SOURCE_MAX_BYTES,
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options
        .open(&source_path)
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "open run script without following links",
            path: source_path.clone(),
            source,
        })?;
    let opened_metadata = file
        .metadata()
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "inspect opened run script",
            path: source_path.clone(),
            source,
        })?;
    if is_link_or_reparse_point(&opened_metadata) || !opened_metadata.is_file() {
        return Err(WorkflowSaveError::InvalidSource {
            path: source_path,
            reason: "opened script.js handle is not a regular file".to_string(),
        });
    }

    let mut bytes = Vec::new();
    file.take(WORKFLOW_SOURCE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| WorkflowSaveError::Io {
            action: "read bounded run script",
            path: source_path.clone(),
            source,
        })?;
    if bytes.len() as u64 > WORKFLOW_SOURCE_MAX_BYTES {
        return Err(WorkflowSaveError::SourceTooLarge {
            path: source_path,
            max_bytes: WORKFLOW_SOURCE_MAX_BYTES,
        });
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|error| WorkflowSaveError::InvalidWorkflowSource {
            path: source_path.clone(),
            reason: format!("source is not valid UTF-8: {error}"),
        })?;
    // `script.js` is the post-`parse_exec_source` code persisted by the runtime,
    // so this is byte-for-byte the same `prompt_hash(exec_args.code)` used when
    // constructing `WorkflowRunMeta::script_hash`.
    let actual_hash = codex_workflow_journal::prompt_hash(text);
    if actual_hash != source_identity.script_hash {
        return Err(WorkflowSaveError::SourceHashMismatch {
            expected: source_identity.script_hash.clone(),
            actual: actual_hash,
        });
    }
    let metadata =
        parse_workflow_meta(text).map_err(|reason| WorkflowSaveError::InvalidWorkflowSource {
            path: source_path,
            reason,
        })?;
    if metadata.name != source_identity.name {
        return Err(WorkflowSaveError::SourceNameMismatch {
            expected: source_identity.name.clone(),
            actual: metadata.name,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "save_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "save_windows_acl_tests.rs"]
mod windows_acl_tests;
