use std::io;

use codex_code_mode_protocol::parse_workflow_meta;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::WalkEntryKind;
use codex_file_system::WalkOptions;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowScope;
use crate::loader::Diagnostics;
use crate::loader::MAX_CANDIDATES_PER_ROOT;
use crate::loader::MAX_DIRECTORIES_PER_ROOT;
use crate::loader::MAX_ENTRIES_PER_ROOT;
use crate::loader::MAX_SCAN_DEPTH;
use crate::loader::META_PHYSICAL_READ_BYTES;
use crate::loader::decode_meta_prefix;

pub(crate) async fn load_workflows_from_executor_root(
    root: &PathUri,
    scope: WorkflowScope,
    file_system: &dyn ExecutorFileSystem,
) -> (Vec<WorkflowMetadata>, Vec<WorkflowLoadError>) {
    let mut diagnostics = Diagnostics::new(root.clone());
    let canonical_root = match file_system.canonicalize(root, /*sandbox*/ None).await {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (Vec::new(), diagnostics.finish());
        }
        Err(error) => {
            diagnostics.push(
                root.clone(),
                format!("failed to resolve workflow root: {error}"),
            );
            return (Vec::new(), diagnostics.finish());
        }
    };
    let walk = match file_system
        .walk(
            &canonical_root,
            WalkOptions {
                max_depth: MAX_SCAN_DEPTH,
                max_directories: MAX_DIRECTORIES_PER_ROOT,
                max_entries: MAX_ENTRIES_PER_ROOT,
                follow_directory_symlinks: false,
                prune_hidden_directories: false,
            },
            /*sandbox*/ None,
        )
        .await
    {
        Ok(walk) => walk,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (Vec::new(), diagnostics.finish());
        }
        Err(error) => {
            diagnostics.push(
                canonical_root,
                format!("failed to scan workflow root: {error}"),
            );
            return (Vec::new(), diagnostics.finish());
        }
    };
    for error in walk.errors {
        diagnostics.push(
            error.path,
            format!("failed to inspect workflow entry: {}", error.message),
        );
    }
    if walk.truncated {
        diagnostics.push(
            canonical_root,
            "workflow traversal limit exceeded; this root was skipped".to_string(),
        );
        return (Vec::new(), diagnostics.finish());
    }

    let mut candidates = walk
        .entries
        .into_iter()
        .filter(|entry| entry.kind == WalkEntryKind::File && is_javascript(&entry.path))
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    candidates.sort_by_key(ToString::to_string);
    if candidates.len() > MAX_CANDIDATES_PER_ROOT {
        diagnostics.push(
            canonical_root,
            format!(
                "workflow candidate limit {MAX_CANDIDATES_PER_ROOT} exceeded; this root was skipped"
            ),
        );
        return (Vec::new(), diagnostics.finish());
    }

    let mut workflows = Vec::new();
    for candidate in candidates {
        match load_workflow(file_system, &candidate, &canonical_root, scope).await {
            Ok(workflow) => workflows.push(workflow),
            Err(error) => diagnostics.push(error.path, error.message),
        }
    }
    (workflows, diagnostics.finish())
}

async fn load_workflow(
    file_system: &dyn ExecutorFileSystem,
    path: &PathUri,
    canonical_root: &PathUri,
    scope: WorkflowScope,
) -> Result<WorkflowMetadata, WorkflowLoadError> {
    if !path.starts_with(canonical_root) {
        return Err(WorkflowLoadError {
            path: path.clone(),
            message: "workflow candidate is outside its root".to_string(),
        });
    }
    let metadata = file_system
        .get_metadata(path, /*sandbox*/ None)
        .await
        .map_err(|error| WorkflowLoadError {
            path: path.clone(),
            message: format!("failed to inspect workflow candidate: {error}"),
        })?;
    if metadata.is_symlink || !metadata.is_file {
        return Err(WorkflowLoadError {
            path: path.clone(),
            message: "workflow candidate is not a regular file".to_string(),
        });
    }
    let canonical = file_system
        .canonicalize(path, /*sandbox*/ None)
        .await
        .map_err(|error| WorkflowLoadError {
            path: path.clone(),
            message: format!("failed to resolve workflow candidate: {error}"),
        })?;
    if !canonical.starts_with(canonical_root) {
        return Err(WorkflowLoadError {
            path: path.clone(),
            message: "workflow candidate resolves outside its root".to_string(),
        });
    }
    let source = read_meta_prefix(file_system, &canonical)
        .await
        .map_err(|error| WorkflowLoadError {
            path: canonical.clone(),
            message: format!("failed to read workflow metadata: {error}"),
        })?;
    let meta = parse_workflow_meta(&source).map_err(|message| WorkflowLoadError {
        path: canonical.clone(),
        message,
    })?;
    Ok(WorkflowMetadata {
        name: meta.name,
        description: meta.description,
        phases: meta.phases,
        path: canonical,
        scope,
    })
}

async fn read_meta_prefix(
    file_system: &dyn ExecutorFileSystem,
    path: &PathUri,
) -> io::Result<String> {
    let mut stream = file_system.read_file_stream(path, /*sandbox*/ None).await?;
    let mut bytes = Vec::with_capacity(META_PHYSICAL_READ_BYTES);
    while bytes.len() < META_PHYSICAL_READ_BYTES
        && let Some(chunk) = stream.next().await
    {
        let chunk = chunk?;
        if chunk.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow metadata stream returned an empty chunk",
            ));
        }
        let remaining = META_PHYSICAL_READ_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    decode_meta_prefix(&bytes)
}

fn is_javascript(path: &PathUri) -> bool {
    path.basename()
        .and_then(|name| {
            name.rsplit_once('.')
                .map(|(stem, extension)| !stem.is_empty() && extension.eq_ignore_ascii_case("js"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "executor_loader_tests.rs"]
mod tests;
