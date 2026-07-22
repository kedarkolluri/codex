use std::error::Error;
use std::fmt;
use std::sync::Arc;

use codex_code_mode_protocol::WORKFLOW_SOURCE_MAX_BYTES;
use codex_code_mode_protocol::parse_workflow_meta;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;

use crate::WorkflowMetadata;
use crate::WorkflowRegistry;

/// One bounded source value captured from an exact saved-workflow registry entry.
///
/// Callers should execute [`Self::source`] directly instead of reopening [`WorkflowMetadata::path`].
/// A later invocation may capture a newer file while an active run keeps this immutable value.
#[derive(Clone, Eq, PartialEq)]
pub struct WorkflowSourceSnapshot {
    metadata: WorkflowMetadata,
    source: Arc<str>,
}

impl WorkflowSourceSnapshot {
    /// The exact registry metadata revalidated against this source.
    pub fn metadata(&self) -> &WorkflowMetadata {
        &self.metadata
    }

    /// The complete UTF-8 source captured for one invocation.
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl fmt::Debug for WorkflowSourceSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowSourceSnapshot")
            .field("metadata", &self.metadata)
            .field("source_bytes", &self.source.len())
            .finish()
    }
}

/// Failure to capture source through a discovered workflow's selected filesystem authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSourceLoadError {
    path: PathUri,
    message: String,
}

impl WorkflowSourceLoadError {
    fn new(path: PathUri, message: impl Into<String>) -> Self {
        Self {
            path,
            message: message.into(),
        }
    }

    /// Registry path whose source could not be captured.
    pub fn path(&self) -> &PathUri {
        &self.path
    }

    /// Diagnostic that never includes workflow source text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for WorkflowSourceLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to load workflow source {}: {}",
            self.path, self.message
        )
    }
}

impl Error for WorkflowSourceLoadError {}

impl WorkflowRegistry {
    /// Captures the source for the exact case-sensitive registry winner named by `name`.
    ///
    /// Executor-backed entries fail closed until source capture through their retained filesystem
    /// authority is available. Host capture freezes one bounded read but does not claim race-free
    /// path containment. Observed I/O, file-kind, path, size, UTF-8, or metadata drift fails closed.
    pub async fn source_snapshot_by_name(
        &self,
        name: &str,
    ) -> Result<Option<WorkflowSourceSnapshot>, WorkflowSourceLoadError> {
        let Some(metadata) = self.resolve_by_name(name).cloned() else {
            return Ok(None);
        };
        if self.executor_file_system_for(&metadata).is_some() {
            return Err(WorkflowSourceLoadError::new(
                metadata.path.clone(),
                "executor-backed workflow source capture is not supported",
            ));
        }
        let bytes = read_host_source(&metadata.path).await?;
        let source = String::from_utf8(bytes).map_err(|error| {
            WorkflowSourceLoadError::new(
                metadata.path.clone(),
                format!("workflow source is not valid UTF-8: {error}"),
            )
        })?;
        let parsed = parse_workflow_meta(&source).map_err(|message| {
            WorkflowSourceLoadError::new(
                metadata.path.clone(),
                format!("workflow metadata is no longer valid: {message}"),
            )
        })?;
        if parsed.name != metadata.name
            || parsed.description != metadata.description
            || parsed.phases != metadata.phases
        {
            return Err(WorkflowSourceLoadError::new(
                metadata.path.clone(),
                "workflow metadata changed after discovery",
            ));
        }
        Ok(Some(WorkflowSourceSnapshot {
            metadata,
            source: Arc::from(source),
        }))
    }
}

async fn read_host_source(path: &PathUri) -> Result<Vec<u8>, WorkflowSourceLoadError> {
    let native_path = path.to_abs_path().map_err(|error| {
        WorkflowSourceLoadError::new(
            path.clone(),
            format!("source is not representable on the host: {error}"),
        )
    })?;
    validate_host_source(path, &native_path).await?;

    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options
        .open(native_path.as_path())
        .await
        .map_err(|error| source_io_error(path, "open source", error))?;
    let opened_metadata = file
        .metadata()
        .await
        .map_err(|error| source_io_error(path, "inspect opened source", error))?;
    if is_link_or_reparse_point(&opened_metadata) || !opened_metadata.is_file() {
        return Err(WorkflowSourceLoadError::new(
            path.clone(),
            "opened workflow source is not a regular, non-reparse file",
        ));
    }
    if opened_metadata.len() > WORKFLOW_SOURCE_MAX_BYTES as u64 {
        return Err(source_too_large(path));
    }

    let read_limit = (WORKFLOW_SOURCE_MAX_BYTES + 1) as u64;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| source_io_error(path, "read source", error))?;
    if bytes.len() > WORKFLOW_SOURCE_MAX_BYTES {
        return Err(source_too_large(path));
    }
    validate_host_source(path, &native_path).await?;
    Ok(bytes)
}

async fn validate_host_source(
    path: &PathUri,
    native_path: &AbsolutePathBuf,
) -> Result<(), WorkflowSourceLoadError> {
    let metadata = tokio::fs::symlink_metadata(native_path.as_path())
        .await
        .map_err(|error| source_io_error(path, "inspect source", error))?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(WorkflowSourceLoadError::new(
            path.clone(),
            "workflow source is not a regular, non-symlink file",
        ));
    }
    if metadata.len() > WORKFLOW_SOURCE_MAX_BYTES as u64 {
        return Err(source_too_large(path));
    }
    let canonical = tokio::fs::canonicalize(native_path.as_path())
        .await
        .and_then(PathUri::from_host_native_path)
        .map_err(|error| source_io_error(path, "resolve source", error))?;
    if canonical != *path {
        return Err(WorkflowSourceLoadError::new(
            path.clone(),
            format!("workflow source now resolves to {canonical}"),
        ));
    }
    Ok(())
}

fn source_too_large(path: &PathUri) -> WorkflowSourceLoadError {
    WorkflowSourceLoadError::new(
        path.clone(),
        format!("workflow source exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"),
    )
}

fn source_io_error(path: &PathUri, action: &str, error: std::io::Error) -> WorkflowSourceLoadError {
    let kind = error.kind();
    WorkflowSourceLoadError::new(path.clone(), format!("failed to {action} ({kind:?})"))
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
