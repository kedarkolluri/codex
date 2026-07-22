use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use codex_file_system::CopyOptions;
use codex_file_system::CreateDirectoryOptions;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::ExecutorFileSystemFuture;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemReadStream;
use codex_file_system::FileSystemResult;
use codex_file_system::FileSystemSandboxContext;
use codex_file_system::ReadDirectoryEntry;
use codex_file_system::RemoveOptions;
use codex_file_system::WalkEntry;
use codex_file_system::WalkEntryKind;
use codex_file_system::WalkError;
use codex_file_system::WalkOptions;
use codex_file_system::WalkOutcome;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::stream;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowRoot;
use crate::WorkflowScope;
use crate::load_workflows_from_roots;

#[derive(Clone, Debug, Eq, PartialEq)]
enum FileSystemCall {
    Canonicalize(PathUri),
    Metadata(PathUri),
    Walk(PathUri, WalkOptions),
    Read(PathUri),
}

struct SyntheticFileSystem {
    root: PathUri,
    canonical: HashMap<PathUri, PathUri>,
    files: HashMap<PathUri, Vec<u8>>,
    walk: WalkOutcome,
    calls: Arc<Mutex<Vec<FileSystemCall>>>,
}

impl SyntheticFileSystem {
    fn unsupported<T>() -> FileSystemResult<T> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "read only"))
    }

    fn metadata(is_directory: bool, is_file: bool) -> FileMetadata {
        FileMetadata {
            is_directory,
            is_file,
            is_symlink: false,
            size: 0,
            created_at_ms: 0,
            modified_at_ms: 0,
        }
    }

    fn record(&self, call: FileSystemCall) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }
}

impl ExecutorFileSystem for SyntheticFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            self.record(FileSystemCall::Canonicalize(path.clone()));
            if path == &self.root || self.files.contains_key(path) {
                Ok(path.clone())
            } else if let Some(canonical) = self.canonical.get(path) {
                Ok(canonical.clone())
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
            }
        })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            self.record(FileSystemCall::Read(path.clone()));
            let contents = self
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
            Ok(FileSystemReadStream::new(stream::iter([Ok(Bytes::from(
                contents,
            ))])))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            self.record(FileSystemCall::Metadata(path.clone()));
            if path == &self.root {
                Ok(Self::metadata(true, false))
            } else if self.files.contains_key(path) || self.canonical.contains_key(path) {
                Ok(Self::metadata(false, true))
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
            }
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async { Self::unsupported() })
    }

    fn walk<'a>(
        &'a self,
        path: &'a PathUri,
        options: WalkOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(async move {
            self.record(FileSystemCall::Walk(path.clone(), options));
            (path == &self.root)
                .then(|| self.walk.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }
}

fn foreign_root() -> PathUri {
    let uri = if cfg!(windows) {
        "file:///workspace/project/.codex/workflows"
    } else {
        "file:///C:/workspace/project/.codex/workflows"
    };
    let root = PathUri::parse(uri).expect("foreign workflow root URI");
    assert!(root.to_abs_path().is_err());
    root
}

fn source(name: &str) -> Vec<u8> {
    format!("export const meta = {{ name: '{name}', description: 'test', phases: [] }};")
        .into_bytes()
}

fn file(path: PathUri) -> WalkEntry {
    WalkEntry {
        path,
        kind: WalkEntryKind::File,
    }
}

#[tokio::test]
async fn foreign_discovery_uses_executor_and_keeps_safe_neighbors() {
    let root = foreign_root();
    let bad = root.join("bad.js").unwrap();
    let escaped = root.join("escaped.js").unwrap();
    let good = root.join("good.JS").unwrap();
    let outside = root.parent().unwrap().join("outside.js").unwrap();
    let walk_error = root.join("unreadable").unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let file_system: Arc<dyn ExecutorFileSystem> = Arc::new(SyntheticFileSystem {
        root: root.clone(),
        canonical: HashMap::from([(escaped.clone(), outside)]),
        files: HashMap::from([
            (bad.clone(), b"export const meta = build();".to_vec()),
            (good.clone(), source("good")),
        ]),
        walk: WalkOutcome {
            entries: vec![file(good.clone()), file(escaped.clone()), file(bad.clone())],
            errors: vec![WalkError {
                path: walk_error.clone(),
                message: "denied".to_string(),
            }],
            truncated: false,
        },
        calls: Arc::clone(&calls),
    });
    let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
        root.clone(),
        Arc::clone(&file_system),
    )])
    .await;
    let expected = WorkflowMetadata {
        name: "good".to_string(),
        description: "test".to_string(),
        phases: Vec::new(),
        path: good.clone(),
        scope: WorkflowScope::Project,
    };
    assert_eq!(registry.workflows(), &[expected.clone()]);
    assert_eq!(
        registry.errors(),
        &[
            WorkflowLoadError {
                path: bad.clone(),
                message: "`meta` must be a static object literal beginning with `{`".to_string(),
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
    assert!(Arc::ptr_eq(
        registry
            .executor_file_system_for(&expected)
            .expect("retained executor filesystem"),
        &file_system,
    ));
    assert_eq!(
        *calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            FileSystemCall::Canonicalize(root.clone()),
            FileSystemCall::Walk(
                root,
                WalkOptions {
                    max_depth: 6,
                    max_directories: 2_000,
                    max_entries: 20_000,
                    follow_directory_symlinks: false,
                    prune_hidden_directories: false,
                },
            ),
            FileSystemCall::Metadata(bad.clone()),
            FileSystemCall::Canonicalize(bad.clone()),
            FileSystemCall::Read(bad),
            FileSystemCall::Metadata(escaped.clone()),
            FileSystemCall::Canonicalize(escaped),
            FileSystemCall::Metadata(good.clone()),
            FileSystemCall::Canonicalize(good.clone()),
            FileSystemCall::Read(good),
        ]
    );
}

#[tokio::test]
async fn identical_uri_spelling_on_distinct_filesystems_is_not_deduplicated() {
    let temp = TempDir::new().unwrap();
    let root = AbsolutePathBuf::from_absolute_path_checked(temp.path()).unwrap();
    let candidate = root.join("same.js");
    std::fs::write(candidate.as_path(), source("host")).unwrap();
    let root_uri = PathUri::from_abs_path(&root);
    let candidate_uri = PathUri::from_abs_path(&candidate);
    let file_system: Arc<dyn ExecutorFileSystem> = Arc::new(SyntheticFileSystem {
        root: root_uri.clone(),
        canonical: HashMap::new(),
        files: HashMap::from([(candidate_uri.clone(), source("executor"))]),
        walk: WalkOutcome {
            entries: vec![file(candidate_uri.clone())],
            errors: Vec::new(),
            truncated: false,
        },
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let registry = load_workflows_from_roots([
        WorkflowRoot::project_on_executor(root_uri, Arc::clone(&file_system)),
        WorkflowRoot::new(root, WorkflowScope::CodexHome),
    ])
    .await;
    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        vec!["executor", "host"]
    );
    let executor = registry.resolve_by_name("executor").unwrap();
    let host = registry.resolve_by_name("host").unwrap();
    assert!(Arc::ptr_eq(
        registry.executor_file_system_for(executor).unwrap(),
        &file_system,
    ));
    assert!(registry.executor_file_system_for(host).is_none());
}

#[tokio::test]
async fn truncated_and_over_candidate_executor_roots_are_discarded() {
    let root = foreign_root();
    for (walk, message) in [
        (
            WalkOutcome {
                entries: vec![file(root.join("valid.js").unwrap())],
                errors: Vec::new(),
                truncated: true,
            },
            "workflow traversal limit exceeded; this root was skipped".to_string(),
        ),
        (
            WalkOutcome {
                entries: (0..=256)
                    .map(|index| file(root.join(&format!("{index}.js")).unwrap()))
                    .collect(),
                errors: Vec::new(),
                truncated: false,
            },
            "workflow candidate limit 256 exceeded; this root was skipped".to_string(),
        ),
    ] {
        let file_system: Arc<dyn ExecutorFileSystem> = Arc::new(SyntheticFileSystem {
            root: root.clone(),
            canonical: HashMap::new(),
            files: HashMap::new(),
            walk,
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
            root.clone(),
            file_system,
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
    }
}
