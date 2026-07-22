use std::error::Error;
use std::fmt;
use std::sync::Arc;

use codex_code_mode_protocol::WORKFLOW_SOURCE_MAX_BYTES;
use codex_code_mode_protocol::parse_workflow_meta;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::VerifiedFileReadOptions;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;

use crate::WorkflowMetadata;
use crate::WorkflowRegistry;

mod host_source;

use host_source::read_host_source;

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
    /// Executor-backed entries stay on their retained filesystem even when their URI could be
    /// represented on this host. They require the verified-read capability and never fall back to
    /// ordinary or host reads. Host capture binds the bytes to one opened object and checks
    /// observed file state and bytes for stability; portable mutable filesystems do not provide a
    /// universal read transaction. Observed I/O, file-kind, identity, path, size, UTF-8, or
    /// metadata drift fails closed.
    pub async fn source_snapshot_by_name(
        &self,
        name: &str,
    ) -> Result<Option<WorkflowSourceSnapshot>, WorkflowSourceLoadError> {
        let Some(metadata) = self.resolve_by_name(name).cloned() else {
            return Ok(None);
        };
        let bytes = match self.executor_file_system_for(&metadata) {
            Some(file_system) => read_executor_source(file_system.as_ref(), &metadata.path).await?,
            None => read_host_source(&metadata.path).await?,
        };
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

async fn read_executor_source(
    file_system: &dyn ExecutorFileSystem,
    path: &PathUri,
) -> Result<Vec<u8>, WorkflowSourceLoadError> {
    let capture = file_system
        .read_file_verified(
            path,
            VerifiedFileReadOptions {
                max_bytes: WORKFLOW_SOURCE_MAX_BYTES as u64,
            },
            /*sandbox*/ None,
        )
        .await
        .map_err(|error| source_io_error(path, "capture source", error))?;
    if capture.size > WORKFLOW_SOURCE_MAX_BYTES as u64 {
        return Err(source_too_large(path));
    }
    let expected_size = usize::try_from(capture.size).map_err(|_| source_too_large(path))?;
    let mut stream = capture.stream;
    let mut bytes = Vec::with_capacity(expected_size);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| source_io_error(path, "read captured source", error))?;
        if chunk.is_empty() {
            return Err(WorkflowSourceLoadError::new(
                path.clone(),
                "verified source stream returned an empty chunk",
            ));
        }
        let Some(next_len) = bytes.len().checked_add(chunk.len()) else {
            return Err(source_too_large(path));
        };
        if next_len > WORKFLOW_SOURCE_MAX_BYTES {
            return Err(source_too_large(path));
        }
        if next_len > expected_size {
            return Err(verified_size_mismatch(path));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != expected_size {
        return Err(verified_size_mismatch(path));
    }
    Ok(bytes)
}

fn verified_size_mismatch(path: &PathUri) -> WorkflowSourceLoadError {
    WorkflowSourceLoadError::new(
        path.clone(),
        "verified source stream did not match its declared size",
    )
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

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "executor_source_tests.rs"]
mod executor_tests;
