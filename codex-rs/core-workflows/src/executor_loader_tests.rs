use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use codex_file_system as fs;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;
use futures::stream;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowRegistry;
use crate::WorkflowRoot;
use crate::WorkflowScope;
use crate::load_workflows_from_roots;

#[derive(Clone, Debug, Eq, PartialEq)]
enum FileSystemCall {
    Canonicalize(PathUri),
    Metadata(PathUri),
    Walk(PathUri, fs::WalkOptions),
    Read(PathUri),
}

struct SyntheticFileSystem {
    walk_root: PathUri,
    canonical: HashMap<PathUri, PathUri>,
    files: HashMap<PathUri, Vec<Bytes>>,
    walk: fs::WalkOutcome,
    calls: Arc<Mutex<Vec<FileSystemCall>>>,
}

impl SyntheticFileSystem {
    fn new(walk_root: PathUri) -> Self {
        Self {
            walk_root,
            canonical: HashMap::new(),
            files: HashMap::new(),
            walk: fs::WalkOutcome::default(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn unsupported<'a, T>() -> fs::ExecutorFileSystemFuture<'a, T> {
        Box::pin(async { Err(io::ErrorKind::Unsupported.into()) })
    }

    fn record(&self, call: FileSystemCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl fs::ExecutorFileSystem for SyntheticFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Canonicalize(path.clone()));
            self.canonical
                .get(path)
                .cloned()
                .or_else(|| {
                    (path == &self.walk_root || self.files.contains_key(path)).then(|| path.clone())
                })
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<u8>> {
        Self::unsupported()
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileSystemReadStream> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Read(path.clone()));
            let chunks = self
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
            Ok(fs::FileSystemReadStream::new(stream::iter(
                chunks.into_iter().map(Ok),
            )))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::CreateDirectoryOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileMetadata> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Metadata(path.clone()));
            (self.files.contains_key(path) || self.canonical.contains_key(path))
                .then_some(fs::FileMetadata {
                    is_directory: false,
                    is_file: true,
                    is_symlink: false,
                    size: 0,
                    created_at_ms: 0,
                    modified_at_ms: 0,
                })
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<fs::ReadDirectoryEntry>> {
        Self::unsupported()
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: fs::WalkOptions,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::WalkOutcome> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Walk(path.clone(), options));
            (path == &self.walk_root)
                .then(|| self.walk.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::RemoveOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: fs::CopyOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unsupported()
    }
}

fn foreign_root() -> PathUri {
    let uri = match PathConvention::native() {
        PathConvention::Posix => "file:///C:/workspace/project/.codex/workflows",
        PathConvention::Windows => "file:///workspace/project/.codex/workflows",
    };
    PathUri::parse(uri).expect("foreign workflow root URI")
}

fn file(path: PathUri) -> fs::WalkEntry {
    fs::WalkEntry {
        path,
        kind: fs::WalkEntryKind::File,
    }
}

fn walk_options() -> fs::WalkOptions {
    fs::WalkOptions {
        max_depth: 6,
        max_directories: 2_000,
        max_entries: 20_000,
        follow_directory_symlinks: false,
        prune_hidden_directories: false,
    }
}

fn workflow_source(name: &str) -> Vec<Bytes> {
    vec![Bytes::from(format!(
        "export const meta = {{ name: '{name}', description: 'test', phases: [] }};"
    ))]
}

fn workflow(path: PathUri, name: &str) -> WorkflowMetadata {
    WorkflowMetadata {
        name: name.to_string(),
        description: "test".to_string(),
        phases: Vec::new(),
        path,
        scope: WorkflowScope::Project,
    }
}

fn assert_executor(
    registry: &WorkflowRegistry,
    workflow: &WorkflowMetadata,
    expected: &Arc<dyn fs::ExecutorFileSystem>,
) {
    let actual = registry.executor_file_system_for(workflow).unwrap();
    assert!(Arc::ptr_eq(&actual, expected));
}

#[tokio::test]
async fn foreign_discovery_stays_on_executor_and_keeps_safe_neighbors() {
    let alias_root = foreign_root();
    let canonical_root = alias_root.parent().unwrap().join("resolved").unwrap();
    let escaped = canonical_root.join("escaped.js").unwrap();
    let good = canonical_root.join("good.JS").unwrap();
    let outside = canonical_root.parent().unwrap().join("outside.js").unwrap();
    let walk_error = canonical_root.join("unreadable").unwrap();
    let mut synthetic = SyntheticFileSystem::new(canonical_root.clone());
    synthetic.canonical.extend([
        (alias_root.clone(), canonical_root.clone()),
        (escaped.clone(), outside.clone()),
    ]);
    synthetic
        .files
        .insert(good.clone(), workflow_source("good"));
    synthetic.walk = fs::WalkOutcome {
        entries: vec![
            file(good.clone()),
            file(escaped.clone()),
            file(outside.clone()),
        ],
        errors: vec![fs::WalkError {
            path: walk_error.clone(),
            message: "denied".to_string(),
        }],
        truncated: false,
    };
    let calls = Arc::clone(&synthetic.calls);
    let file_system: Arc<dyn fs::ExecutorFileSystem> = Arc::new(synthetic);

    let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
        alias_root.clone(),
        Arc::clone(&file_system),
    )])
    .await;
    let expected = workflow(good.clone(), "good");
    assert_eq!(registry.workflows(), std::slice::from_ref(&expected));
    assert_eq!(
        registry.errors(),
        &[
            WorkflowLoadError {
                path: outside,
                message: "workflow candidate is outside its root".to_string(),
            },
            WorkflowLoadError {
                path: escaped.clone(),
                message: "workflow candidate resolves outside its root".to_string(),
            },
            WorkflowLoadError {
                path: walk_error,
                message: "failed to inspect workflow entry: denied".to_string(),
            },
        ]
    );
    assert_executor(&registry, &expected, &file_system);
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![
            FileSystemCall::Canonicalize(alias_root),
            FileSystemCall::Walk(canonical_root, walk_options()),
            FileSystemCall::Metadata(escaped.clone()),
            FileSystemCall::Canonicalize(escaped),
            FileSystemCall::Metadata(good.clone()),
            FileSystemCall::Canonicalize(good.clone()),
            FileSystemCall::Read(good),
        ]
    );
}

#[tokio::test]
async fn host_convertible_executor_root_stays_on_supplied_file_system() {
    let temp_dir = TempDir::new().unwrap();
    let native_root =
        AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("executor-only")).unwrap();
    assert!(!native_root.as_path().exists());
    let root = PathUri::from_abs_path(&native_root);
    assert_eq!(root.to_abs_path().unwrap(), native_root);
    let candidate = root.join("native.js").unwrap();
    let mut synthetic = SyntheticFileSystem::new(root.clone());
    synthetic
        .files
        .insert(candidate.clone(), workflow_source("native"));
    synthetic.walk.entries.push(file(candidate.clone()));
    let calls = Arc::clone(&synthetic.calls);
    let file_system: Arc<dyn fs::ExecutorFileSystem> = Arc::new(synthetic);

    let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
        root.clone(),
        Arc::clone(&file_system),
    )])
    .await;
    let expected = workflow(candidate.clone(), "native");

    assert_eq!(registry.workflows(), std::slice::from_ref(&expected));
    assert_eq!(registry.errors(), &[]);
    assert_executor(&registry, &expected, &file_system);
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![
            FileSystemCall::Canonicalize(root.clone()),
            FileSystemCall::Walk(root, walk_options()),
            FileSystemCall::Metadata(candidate.clone()),
            FileSystemCall::Canonicalize(candidate.clone()),
            FileSystemCall::Read(candidate),
        ]
    );
}

#[tokio::test]
async fn truncated_and_over_candidate_executor_roots_are_discarded() {
    let root = foreign_root();
    for (walk, message) in [
        (
            fs::WalkOutcome {
                entries: vec![file(root.join("valid.js").unwrap())],
                errors: Vec::new(),
                truncated: true,
            },
            "workflow traversal limit exceeded; this root was skipped".to_string(),
        ),
        (
            fs::WalkOutcome {
                entries: (0..=256)
                    .map(|index| file(root.join(&format!("{index}.js")).unwrap()))
                    .collect(),
                errors: Vec::new(),
                truncated: false,
            },
            "workflow candidate limit 256 exceeded; this root was skipped".to_string(),
        ),
    ] {
        let mut synthetic = SyntheticFileSystem::new(root.clone());
        synthetic.walk = walk;
        let calls = Arc::clone(&synthetic.calls);
        let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
            root.clone(),
            Arc::new(synthetic),
        )])
        .await;
        assert_eq!(registry.workflows(), &[]);
        assert_eq!(
            registry.errors(),
            &[WorkflowLoadError {
                path: root.clone(),
                message,
            }]
        );
        assert_eq!(
            calls.lock().unwrap().clone(),
            vec![
                FileSystemCall::Canonicalize(root.clone()),
                FileSystemCall::Walk(root.clone(), walk_options()),
            ]
        );
    }
}
