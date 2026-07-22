use std::collections::HashSet;
use std::io;

use codex_code_mode_protocol::WORKFLOW_META_MAX_BYTES;
use codex_code_mode_protocol::parse_workflow_meta;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::AsyncReadExt;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowRegistry;
use crate::WorkflowRoot;
use crate::WorkflowScope;

const MAX_WORKFLOW_ROOTS: usize = 8;
const MAX_SCAN_DEPTH: usize = 6;
const MAX_DIRECTORIES_PER_ROOT: usize = 2_000;
const MAX_ENTRIES_PER_ROOT: usize = 20_000;
const MAX_CANDIDATES_PER_ROOT: usize = 256;
const MAX_DIAGNOSTICS_PER_ROOT: usize = 64;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1_024;

// Preserve parser lookahead so truncation cannot create an artificial metadata EOF; three extra
// bytes prove a UTF-8 scalar split at the read horizon.
const META_PARSER_LOOKAHEAD_BYTES: usize = 4 * 1_024;
const UTF8_BOUNDARY_LOOKAHEAD_BYTES: usize = 3;
const META_LOGICAL_READ_BYTES: usize = WORKFLOW_META_MAX_BYTES + META_PARSER_LOOKAHEAD_BYTES;
const META_PHYSICAL_READ_BYTES: usize = META_LOGICAL_READ_BYTES + UTF8_BOUNDARY_LOOKAHEAD_BYTES;

#[derive(Clone, Copy)]
struct DiscoveryLimits {
    max_depth: usize,
    max_directories: usize,
    max_entries: usize,
    max_candidates: usize,
}

const DISCOVERY_LIMITS: DiscoveryLimits = DiscoveryLimits {
    max_depth: MAX_SCAN_DEPTH,
    max_directories: MAX_DIRECTORIES_PER_ROOT,
    max_entries: MAX_ENTRIES_PER_ROOT,
    max_candidates: MAX_CANDIDATES_PER_ROOT,
};

struct RootScan {
    canonical_root: AbsolutePathBuf,
    candidates: Vec<AbsolutePathBuf>,
    diagnostics: Diagnostics,
}

impl RootScan {
    fn empty(canonical_root: AbsolutePathBuf, diagnostics: Diagnostics) -> Self {
        Self {
            canonical_root,
            candidates: Vec::new(),
            diagnostics,
        }
    }

    fn exceeded(
        canonical_root: AbsolutePathBuf,
        mut diagnostics: Diagnostics,
        limit: usize,
        unit: &str,
    ) -> Self {
        diagnostics.push(
            canonical_root.clone(),
            format!("workflow {unit} limit {limit} exceeded; this root was skipped"),
        );
        Self::empty(canonical_root, diagnostics)
    }
}

struct Diagnostics {
    root: AbsolutePathBuf,
    errors: Vec<WorkflowLoadError>,
    omitted: usize,
}

impl Diagnostics {
    fn new(root: AbsolutePathBuf) -> Self {
        Self {
            root,
            errors: Vec::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, path: AbsolutePathBuf, mut message: String) {
        if message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
            let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES - 3;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push_str("...");
        }
        if self.errors.len() < MAX_DIAGNOSTICS_PER_ROOT - 1 {
            self.errors.push(WorkflowLoadError { path, message });
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn finish(mut self) -> Vec<WorkflowLoadError> {
        if self.omitted > 0 {
            let omitted = self.omitted;
            self.errors.push(WorkflowLoadError {
                path: self.root,
                message: format!("{omitted} additional workflow discovery diagnostic(s) omitted"),
            });
        }
        self.errors
    }
}

/// Discovers host-local saved workflows without evaluating their JavaScript bodies.
///
/// Missing roots are ignored. Descendant symlinks are not followed; an explicit root alias is
/// resolved once. Other scan, containment, read, UTF-8, and static metadata failures become
/// bounded diagnostics while discovery continues with safe neighbors.
pub async fn load_workflows_from_roots<I>(roots: I) -> WorkflowRegistry
where
    I: IntoIterator<Item = WorkflowRoot>,
{
    let mut roots = roots
        .into_iter()
        .take(MAX_WORKFLOW_ROOTS + 1)
        .collect::<Vec<_>>();
    let mut workflows = Vec::new();
    let mut errors = Vec::new();
    if roots.len() > MAX_WORKFLOW_ROOTS {
        errors.push(WorkflowLoadError {
            path: roots[MAX_WORKFLOW_ROOTS].path.clone(),
            message: format!("workflow root limit {MAX_WORKFLOW_ROOTS} exceeded; extras ignored"),
        });
        roots.truncate(MAX_WORKFLOW_ROOTS);
    }

    for root in roots {
        let RootScan {
            canonical_root,
            candidates,
            mut diagnostics,
        } = scan_workflow_root(&root, DISCOVERY_LIMITS).await;
        for candidate in candidates {
            match load_workflow(&candidate, &canonical_root, root.scope).await {
                Ok(workflow) => workflows.push(workflow),
                Err(error) => diagnostics.push(error.path, error.message),
            }
        }
        errors.extend(diagnostics.finish());
    }

    WorkflowRegistry::new(workflows, errors)
}

async fn scan_workflow_root(root: &WorkflowRoot, limits: DiscoveryLimits) -> RootScan {
    let mut diagnostics = Diagnostics::new(root.path.clone());
    let canonical_root = match canonicalize(&root.path).await {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return RootScan::empty(root.path.clone(), diagnostics);
        }
        Err(error) => {
            diagnostics.push(
                root.path.clone(),
                format!("failed to resolve workflow root: {error}"),
            );
            return RootScan::empty(root.path.clone(), diagnostics);
        }
    };
    let mut pending = vec![(canonical_root.clone(), 0_usize)];
    let mut seen_directories = HashSet::from([canonical_root.clone()]);
    let mut entries_seen = 0_usize;
    let mut candidates = Vec::new();
    while let Some((directory, depth)) = pending.pop() {
        let mut reader = match tokio::fs::read_dir(directory.as_path()).await {
            Ok(reader) => reader,
            Err(error) => {
                diagnostics.push(
                    directory,
                    format!("failed to read workflow directory: {error}"),
                );
                continue;
            }
        };
        let mut entries = Vec::new();
        loop {
            let entry = match reader.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    diagnostics.push(
                        directory.clone(),
                        format!("failed while reading workflow directory: {error}"),
                    );
                    break;
                }
            };
            if entries_seen == limits.max_entries {
                let limit = limits.max_entries;
                return RootScan::exceeded(canonical_root, diagnostics, limit, "entry");
            }
            entries_seen += 1;
            let path = match AbsolutePathBuf::from_absolute_path_checked(entry.path()) {
                Ok(path) => path,
                Err(error) => {
                    diagnostics.push(
                        directory.clone(),
                        format!("workflow entry did not have an absolute path: {error}"),
                    );
                    continue;
                }
            };
            match entry.file_type().await {
                Ok(file_type) => entries.push((path, file_type)),
                Err(error) => {
                    diagnostics.push(path, format!("failed to inspect workflow entry: {error}"))
                }
            }
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        let mut child_directories = Vec::new();
        for (path, file_type) in entries {
            if file_type.is_symlink() {
                continue;
            }
            let direct_codex_runs = root.scope == WorkflowScope::CodexHome
                && depth == 0
                && file_type.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("runs"));
            if direct_codex_runs {
                continue;
            }
            if file_type.is_dir() {
                let child_depth = depth + 1;
                if child_depth > limits.max_depth {
                    diagnostics.push(
                        path,
                        format!(
                            "workflow scan reached the {max_depth}-level depth limit; this subtree was skipped",
                            max_depth = limits.max_depth
                        ),
                    );
                    continue;
                }
                let canonical = match canonicalize(&path).await {
                    Ok(canonical) if canonical.starts_with(canonical_root.as_path()) => canonical,
                    Ok(_) => {
                        diagnostics.push(
                            path,
                            "workflow directory resolves outside its root".to_string(),
                        );
                        continue;
                    }
                    Err(error) => {
                        diagnostics.push(
                            path,
                            format!("failed to resolve workflow directory: {error}"),
                        );
                        continue;
                    }
                };
                if seen_directories.insert(canonical.clone()) {
                    if seen_directories.len() > limits.max_directories {
                        let limit = limits.max_directories;
                        return RootScan::exceeded(canonical_root, diagnostics, limit, "directory");
                    }
                    child_directories.push(canonical);
                }
            } else if file_type.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("js"))
            {
                if candidates.len() == limits.max_candidates {
                    let limit = limits.max_candidates;
                    return RootScan::exceeded(canonical_root, diagnostics, limit, "candidate");
                }
                candidates.push(path);
            }
        }
        child_directories.sort();
        pending.extend(
            child_directories
                .into_iter()
                .rev()
                .map(|directory| (directory, depth + 1)),
        );
    }
    candidates.sort();
    RootScan {
        canonical_root,
        candidates,
        diagnostics,
    }
}

async fn load_workflow(
    path: &AbsolutePathBuf,
    canonical_root: &AbsolutePathBuf,
    scope: WorkflowScope,
) -> Result<WorkflowMetadata, WorkflowLoadError> {
    let canonical = canonicalize(path)
        .await
        .map_err(|error| WorkflowLoadError {
            path: path.clone(),
            message: format!("failed to resolve workflow candidate: {error}"),
        })?;
    if !canonical.starts_with(canonical_root.as_path()) {
        return Err(WorkflowLoadError {
            path: path.clone(),
            message: "workflow candidate resolves outside its root".to_string(),
        });
    }
    let source = read_meta_prefix(&canonical)
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

async fn read_meta_prefix(path: &AbsolutePathBuf) -> io::Result<String> {
    let file = tokio::fs::File::open(path.as_path()).await?;
    if !file.metadata().await?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workflow candidate is not a regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(META_PHYSICAL_READ_BYTES);
    file.take(META_PHYSICAL_READ_BYTES as u64)
        .read_to_end(&mut bytes)
        .await?;
    decode_meta_prefix(&bytes)
}

fn decode_meta_prefix(bytes: &[u8]) -> io::Result<String> {
    let logical_len = bytes.len().min(META_LOGICAL_READ_BYTES);
    let logical = &bytes[..logical_len];
    match std::str::from_utf8(logical) {
        Ok(text) => Ok(text.to_string()),
        Err(error) if error.error_len().is_some() => Err(invalid_utf8()),
        Err(error) => {
            let valid = error.valid_up_to();
            let tail = &bytes[valid..];
            let completed = (1..=tail.len().min(4)).find(|length| {
                std::str::from_utf8(&tail[..*length])
                    .ok()
                    .is_some_and(|text| text.chars().count() == 1)
            });
            let Some(completed) = completed else {
                return Err(invalid_utf8());
            };
            String::from_utf8(bytes[..valid + completed].to_vec()).map_err(|_| invalid_utf8())
        }
    }
}

fn invalid_utf8() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "workflow metadata prefix is not valid UTF-8",
    )
}

async fn canonicalize(path: &AbsolutePathBuf) -> io::Result<AbsolutePathBuf> {
    let path = tokio::fs::canonicalize(path.as_path()).await?;
    AbsolutePathBuf::from_absolute_path_checked(path)
}

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;
