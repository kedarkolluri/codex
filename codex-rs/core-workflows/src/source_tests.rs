use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use codex_file_system as fs;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::stream;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::WorkflowSourceSnapshot;
use crate::WORKFLOW_SOURCE_MAX_BYTES;
use crate::WorkflowMetadata;
use crate::WorkflowRegistry;
use crate::WorkflowRoot;
use crate::WorkflowScope;
use crate::model::WorkflowFileSystemAuthority;
use crate::model::WorkflowRootSource;

fn workflow_source(name: &str, description: &str, phases: &str, marker: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: {phases} }};\n// {marker}\n"
    )
}

fn canonical_uri(path: &Path) -> PathUri {
    let path = AbsolutePathBuf::from_absolute_path_checked(path.canonicalize().unwrap()).unwrap();
    PathUri::from_abs_path(&path)
}

fn metadata(path: PathUri) -> WorkflowMetadata {
    WorkflowMetadata {
        name: "review".to_string(),
        description: "Review changes".to_string(),
        phases: vec!["Inspect".to_string(), "Report".to_string()],
        path,
        scope: WorkflowScope::Project,
    }
}

fn host_registry(path: &Path) -> (WorkflowRegistry, WorkflowMetadata) {
    let metadata = metadata(canonical_uri(path));
    (
        WorkflowRegistry::from_host(vec![metadata.clone()], Vec::new()),
        metadata,
    )
}

fn padded_source(marker: &str, len: usize) -> String {
    let mut source = workflow_source("review", "Review changes", "['Inspect', 'Report']", marker);
    source.push_str(&" ".repeat(len - source.len()));
    source
}

#[tokio::test]
async fn host_snapshot_is_exact_immutable_and_redacted() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let original = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "original-secret-body",
    );
    std::fs::write(&path, &original).unwrap();
    let (registry, metadata) = host_registry(&path);

    let snapshot = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot,
        WorkflowSourceSnapshot {
            metadata: metadata.clone(),
            source: Arc::from(original.clone()),
        }
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("original-secret-body"));
    assert!(debug.contains(&format!("source_bytes: {}", original.len())));

    let replacement = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "replacement-body",
    );
    std::fs::write(&path, &replacement).unwrap();
    assert_eq!(snapshot.source(), original);
    assert_eq!(snapshot.metadata(), &metadata);
    assert_eq!(
        registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap()
            .source(),
        replacement
    );
}

#[tokio::test]
async fn host_snapshot_enforces_size_and_utf8() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let exact = padded_source("exact", WORKFLOW_SOURCE_MAX_BYTES);
    std::fs::write(&path, &exact).unwrap();
    let (registry, _) = host_registry(&path);
    assert_eq!(
        registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap()
            .source()
            .len(),
        WORKFLOW_SOURCE_MAX_BYTES
    );

    std::fs::write(&path, format!("{exact}x")).unwrap();
    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("exceeds"));

    let mut invalid = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "invalid",
    )
    .into_bytes();
    invalid.push(0xff);
    std::fs::write(&path, invalid).unwrap();
    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("not valid UTF-8"));

    std::fs::remove_file(&path).unwrap();
    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("failed to inspect source"));
}

#[test]
fn backend_io_error_text_is_redacted() {
    let path = PathUri::parse("file:///workflow.js").unwrap();
    let error = super::source_io_error(&path, "read source", io::Error::other("source-secret"));
    assert_eq!(error.message(), "failed to read source (Other)");
}

#[tokio::test]
async fn host_snapshot_rejects_metadata_drift_without_leaking_source() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let original = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "original",
    );
    std::fs::write(&path, original).unwrap();
    let (registry, _) = host_registry(&path);

    let replacement = workflow_source(
        "deploy",
        "Changed description",
        "['Different']",
        "replacement-secret",
    );
    std::fs::write(&path, replacement).unwrap();
    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_eq!(error.message(), "workflow metadata changed after discovery");
    assert!(!format!("{error:?}").contains("secret"));
    assert!(!error.to_string().contains("secret"));
}

#[cfg(unix)]
#[tokio::test]
async fn host_snapshot_rejects_final_symlink_replacement() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let source = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "original",
    );
    std::fs::write(&path, source).unwrap();
    let (registry, metadata) = host_registry(&path);
    let referent = temp.path().join("referent.js");
    std::fs::write(
        &referent,
        workflow_source(
            "review",
            "Review changes",
            "['Inspect', 'Report']",
            "redirected",
        ),
    )
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&referent, &path).unwrap();

    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_eq!(error.path(), &metadata.path);
    assert!(error.message().contains("non-symlink"));
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FileSystemCall {
    Metadata,
    Canonicalize,
    Read,
}

struct SyntheticFileSystem {
    metadata: fs::FileMetadata,
    canonical: PathUri,
    stream: Vec<Bytes>,
    calls: Arc<Mutex<Vec<FileSystemCall>>>,
}

impl SyntheticFileSystem {
    fn new(path: PathUri, source: impl Into<Bytes>) -> Self {
        let source = source.into();
        Self {
            metadata: fs::FileMetadata {
                is_directory: false,
                is_file: true,
                is_symlink: false,
                size: source.len() as u64,
                created_at_ms: 0,
                modified_at_ms: 0,
            },
            canonical: path,
            stream: vec![source],
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, call: FileSystemCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl fs::ExecutorFileSystem for SyntheticFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Canonicalize);
            Ok(self.canonical.clone())
        })
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("source snapshot must use bounded streaming reads")
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileSystemReadStream> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Read);
            let chunks = self.stream.clone().into_iter().map(Ok);
            Ok(fs::FileSystemReadStream::new(stream::iter(chunks)))
        })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected write")
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::CreateDirectoryOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected create directory")
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileMetadata> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(FileSystemCall::Metadata);
            Ok(self.metadata.clone())
        })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<fs::ReadDirectoryEntry>> {
        panic!("unexpected read directory")
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::RemoveOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected remove")
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: fs::CopyOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("unexpected copy")
    }
}

fn authority(
    path: &PathUri,
    file_system: Arc<dyn fs::ExecutorFileSystem>,
) -> WorkflowFileSystemAuthority {
    let root = WorkflowRoot::project_on_executor(path.parent().unwrap(), file_system);
    let WorkflowRootSource::Executor(authority) = root.source() else {
        unreachable!("executor workflow root changed authority")
    };
    authority.clone()
}

#[tokio::test]
async fn executor_snapshot_uses_only_the_winning_retained_authority() {
    let temp = TempDir::new().unwrap();
    let path = PathUri::from_host_native_path(temp.path().join("host-missing.js")).unwrap();
    let source_a = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "authority-a",
    );
    let source_b = workflow_source(
        "review",
        "Review changes",
        "['Inspect', 'Report']",
        "authority-b",
    );
    let file_system_a = Arc::new(SyntheticFileSystem::new(path.clone(), source_a));
    let file_system_b = Arc::new(SyntheticFileSystem::new(path.clone(), source_b.clone()));
    let calls_a = Arc::clone(&file_system_a.calls);
    let calls_b = Arc::clone(&file_system_b.calls);
    let metadata = metadata(path.clone());
    let authority_b = authority(&path, file_system_b.clone());
    let authority_a = authority(&path, file_system_a.clone());
    let registry = WorkflowRegistry::from_discovery(
        vec![
            (metadata.clone(), Some(authority_b)),
            (metadata.clone(), Some(authority_a)),
        ],
        Vec::new(),
    );
    assert_eq!(registry.source_snapshot_by_name("Review").await, Ok(None));
    assert_eq!(calls_a.lock().unwrap().as_slice(), &[]);
    assert_eq!(calls_b.lock().unwrap().as_slice(), &[]);

    let snapshot = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot,
        WorkflowSourceSnapshot {
            metadata,
            source: Arc::from(source_b),
        }
    );
    assert_eq!(calls_a.lock().unwrap().as_slice(), &[]);
    assert_eq!(
        calls_b.lock().unwrap().as_slice(),
        &[
            FileSystemCall::Metadata,
            FileSystemCall::Canonicalize,
            FileSystemCall::Read,
            FileSystemCall::Metadata,
            FileSystemCall::Canonicalize,
        ]
    );
}

fn executor_registry(file_system: Arc<SyntheticFileSystem>, path: PathUri) -> WorkflowRegistry {
    let entry = metadata(path.clone());
    let file_system: Arc<dyn fs::ExecutorFileSystem> = file_system;
    let authority = authority(&path, file_system);
    WorkflowRegistry::from_discovery(vec![(entry, Some(authority))], Vec::new())
}

#[tokio::test]
async fn executor_snapshot_rejects_stream_and_authority_failures() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let mut oversized = SyntheticFileSystem::new(path.clone(), Bytes::new());
    oversized.stream = vec![
        Bytes::from(vec![b' '; WORKFLOW_SOURCE_MAX_BYTES]),
        Bytes::from_static(b"x"),
    ];
    let error = executor_registry(Arc::new(oversized), path.clone())
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("exceeds"));

    let mut empty = SyntheticFileSystem::new(path.clone(), Bytes::new());
    empty.stream = vec![Bytes::new()];
    let error = executor_registry(Arc::new(empty), path.clone())
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("empty chunk"));

    let mut symlink = SyntheticFileSystem::new(path.clone(), Bytes::new());
    symlink.metadata.is_symlink = true;
    let error = executor_registry(Arc::new(symlink), path.clone())
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("non-symlink"));

    let mut drifted = SyntheticFileSystem::new(path.clone(), Bytes::new());
    drifted.canonical = PathUri::parse("file:///outside/review.js").unwrap();
    let error = executor_registry(Arc::new(drifted), path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert!(error.message().contains("now resolves"));
}
