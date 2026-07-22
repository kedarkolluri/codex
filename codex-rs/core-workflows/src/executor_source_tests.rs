use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use codex_file_system as fs;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;
use futures::Stream;
use futures::stream;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::Notify;

use super::WorkflowSourceLoadError;
use super::WorkflowSourceSnapshot;
use crate::WORKFLOW_SOURCE_MAX_BYTES;
use crate::WorkflowMetadata;
use crate::WorkflowRegistry;
use crate::WorkflowRoot;
use crate::WorkflowScope;
use crate::model::WorkflowFileSystemAuthority;
use crate::model::WorkflowRootSource;

fn workflow_source(marker: &str) -> String {
    format!(
        "export const meta = {{ name: 'review', description: 'Review changes', phases: ['Inspect', 'Report'] }};\n// {marker}\n"
    )
}

fn workflow(path: PathUri) -> WorkflowMetadata {
    WorkflowMetadata {
        name: "review".to_string(),
        description: "Review changes".to_string(),
        phases: vec!["Inspect".to_string(), "Report".to_string()],
        path,
        scope: WorkflowScope::Project,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedReadCall {
    path: PathUri,
    options: fs::VerifiedFileReadOptions,
}

struct SyntheticFileSystem {
    capture_size: u64,
    capture_open_error: Option<io::ErrorKind>,
    stream: Vec<Result<Bytes, io::ErrorKind>>,
    stream_drop_probe: Option<Arc<AtomicBool>>,
    pending_stream_probe: Option<Arc<PendingStreamProbe>>,
    calls: Mutex<Vec<VerifiedReadCall>>,
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct PendingStreamProbe {
    polled: Notify,
    dropped: AtomicBool,
}

struct PendingStream {
    probe: Arc<PendingStreamProbe>,
}

impl Stream for PendingStream {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.probe.polled.notify_one();
        Poll::Pending
    }
}

impl Drop for PendingStream {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

impl SyntheticFileSystem {
    fn new(source: impl Into<Bytes>) -> Self {
        let source = source.into();
        Self {
            capture_size: source.len() as u64,
            capture_open_error: None,
            stream: vec![Ok(source)],
            stream_drop_probe: None,
            pending_stream_probe: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, path: PathUri, options: fs::VerifiedFileReadOptions) {
        self.calls
            .lock()
            .unwrap()
            .push(VerifiedReadCall { path, options });
    }

    fn calls(&self) -> Vec<VerifiedReadCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl fs::ExecutorFileSystem for SyntheticFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, PathUri> {
        panic!("verified source capture must not canonicalize separately")
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("verified source capture must not use ordinary reads")
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileSystemReadStream> {
        panic!("verified source capture must not use ordinary streaming reads")
    }

    fn read_file_verified<'a>(
        &'a self,
        path: &'a PathUri,
        options: fs::VerifiedFileReadOptions,
        sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::VerifiedFileRead> {
        Box::pin(async move {
            assert!(sandbox.is_none());
            self.record(path.clone(), options);
            if let Some(kind) = self.capture_open_error {
                return Err(io::Error::new(kind, "source-secret"));
            }
            if let Some(probe) = &self.pending_stream_probe {
                return Ok(fs::VerifiedFileRead {
                    size: self.capture_size,
                    stream: fs::FileSystemReadStream::new(PendingStream {
                        probe: Arc::clone(probe),
                    }),
                });
            }
            let chunks = self.stream.clone().into_iter();
            let drop_probe = self
                .stream_drop_probe
                .as_ref()
                .map(|probe| DropProbe(Arc::clone(probe)));
            let stream = stream::unfold(
                (chunks, drop_probe),
                |(mut chunks, drop_probe)| async move {
                    let chunk = chunks.next()?;
                    Some((
                        chunk.map_err(|kind| io::Error::new(kind, "source-secret")),
                        (chunks, drop_probe),
                    ))
                },
            );
            Ok(fs::VerifiedFileRead {
                size: self.capture_size,
                stream: fs::FileSystemReadStream::new(stream),
            })
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
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileMetadata> {
        panic!("verified source capture must not inspect metadata separately")
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

fn executor_registry(file_system: Arc<SyntheticFileSystem>, path: &PathUri) -> WorkflowRegistry {
    let entry = workflow(path.clone());
    let file_system: Arc<dyn fs::ExecutorFileSystem> = file_system;
    let authority = authority(path, file_system);
    WorkflowRegistry::from_discovery(vec![(entry, Some(authority))], Vec::new())
}

fn assert_load_error(error: WorkflowSourceLoadError, path: &PathUri, message: &str) {
    assert_eq!(
        error,
        WorkflowSourceLoadError::new(path.clone(), message.to_string())
    );
}

fn verified_read_call(path: &PathUri) -> VerifiedReadCall {
    VerifiedReadCall {
        path: path.clone(),
        options: fs::VerifiedFileReadOptions {
            max_bytes: WORKFLOW_SOURCE_MAX_BYTES as u64,
        },
    }
}

fn remote_native_path() -> PathUri {
    let path = match PathConvention::native() {
        PathConvention::Posix => "file:///C:/remote/workflows/review.js",
        PathConvention::Windows => "file:///remote/workflows/review.js",
    };
    PathUri::parse(path).unwrap()
}

#[tokio::test]
async fn executor_snapshot_uses_winning_authority_and_accepts_exact_limit() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source_a = workflow_source("authority-a");
    let mut source_b = workflow_source("authority-b");
    source_b.push_str(&" ".repeat(WORKFLOW_SOURCE_MAX_BYTES - source_b.len()));
    let file_system_a = Arc::new(SyntheticFileSystem::new(source_a));
    let file_system_b = Arc::new(SyntheticFileSystem::new(source_b.clone()));
    let entry = workflow(path.clone());
    let authority_b = authority(&path, file_system_b.clone());
    let authority_a = authority(&path, file_system_a.clone());
    let registry = WorkflowRegistry::from_discovery(
        vec![
            (entry.clone(), Some(authority_b)),
            (entry.clone(), Some(authority_a)),
        ],
        Vec::new(),
    );

    assert_eq!(registry.source_snapshot_by_name("Review").await, Ok(None));
    assert_eq!(file_system_a.calls(), []);
    assert_eq!(file_system_b.calls(), []);
    assert_eq!(
        registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap(),
        WorkflowSourceSnapshot {
            metadata: entry,
            source: Arc::from(source_b),
        }
    );
    assert_eq!(file_system_a.calls(), []);
    assert_eq!(file_system_b.calls(), [verified_read_call(&path)]);
}

#[tokio::test]
async fn executor_snapshot_keeps_host_convertible_path_on_retained_authority() {
    let temp_dir = TempDir::new().unwrap();
    let native_path = temp_dir.path().join("review.js");
    let host_source = workflow_source("host-bytes-must-not-win");
    std::fs::write(&native_path, host_source).unwrap();
    let path = PathUri::from_host_native_path(std::fs::canonicalize(native_path).unwrap()).unwrap();
    assert!(path.to_abs_path().is_ok());

    let executor_source = workflow_source("executor-bytes");
    let file_system = Arc::new(SyntheticFileSystem::new(executor_source.clone()));
    let entry = workflow(path.clone());
    let registry = executor_registry(Arc::clone(&file_system), &path);

    assert_eq!(
        registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap(),
        WorkflowSourceSnapshot {
            metadata: entry,
            source: Arc::from(executor_source),
        }
    );
    assert_eq!(file_system.calls(), [verified_read_call(&path)]);
}

#[tokio::test]
async fn executor_snapshot_rejects_invalid_verified_stream_shapes() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let mut declared_oversized = SyntheticFileSystem::new(Bytes::new());
    declared_oversized.capture_size = WORKFLOW_SOURCE_MAX_BYTES as u64 + 1;
    let declared_oversized = Arc::new(declared_oversized);
    let error = executor_registry(Arc::clone(&declared_oversized), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_load_error(
        error,
        &path,
        &format!("workflow source exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"),
    );
    assert_eq!(declared_oversized.calls(), [verified_read_call(&path)]);

    let mut streamed_oversized = SyntheticFileSystem::new(Bytes::new());
    streamed_oversized.capture_size = WORKFLOW_SOURCE_MAX_BYTES as u64;
    streamed_oversized.stream = vec![
        Ok(Bytes::from(vec![b' '; WORKFLOW_SOURCE_MAX_BYTES])),
        Ok(Bytes::from_static(b"x")),
    ];
    let streamed_oversized = Arc::new(streamed_oversized);
    let error = executor_registry(Arc::clone(&streamed_oversized), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_load_error(
        error,
        &path,
        &format!("workflow source exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"),
    );
    assert_eq!(streamed_oversized.calls(), [verified_read_call(&path)]);

    let mut empty = SyntheticFileSystem::new(Bytes::new());
    empty.stream = vec![Ok(Bytes::new())];
    let empty = Arc::new(empty);
    let error = executor_registry(Arc::clone(&empty), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_load_error(
        error,
        &path,
        "verified source stream returned an empty chunk",
    );
    assert_eq!(empty.calls(), [verified_read_call(&path)]);

    let source = workflow_source("length-mismatch");
    let mismatches = [
        (source.len() as u64, source[..source.len() - 1].to_string()),
        (source.len() as u64 - 1, source.clone()),
    ];
    for (capture_size, stream) in mismatches {
        let mut file_system = SyntheticFileSystem::new(Bytes::new());
        file_system.capture_size = capture_size;
        file_system.stream = vec![Ok(Bytes::from(stream))];
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(
            error,
            &path,
            "verified source stream did not match its declared size",
        );
        assert_eq!(file_system.calls(), [verified_read_call(&path)]);
    }
}

#[tokio::test]
async fn executor_snapshot_rejects_verified_io_failures_without_legacy_fallback() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source = workflow_source("io-error");
    let cases = [
        (
            Some(io::ErrorKind::PermissionDenied),
            vec![Ok(Bytes::from(source.clone()))],
            "failed to capture source (PermissionDenied)",
        ),
        (
            Some(io::ErrorKind::Unsupported),
            vec![Ok(Bytes::from(source.clone()))],
            "failed to capture source (Unsupported)",
        ),
        (
            None,
            vec![
                Ok(Bytes::from_static(b"partial")),
                Err(io::ErrorKind::ConnectionReset),
            ],
            "failed to read captured source (ConnectionReset)",
        ),
    ];
    for (capture_open_error, stream, message) in cases {
        let mut file_system = SyntheticFileSystem::new(source.clone());
        file_system.capture_open_error = capture_open_error;
        file_system.stream = stream;
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, message);
        assert_eq!(file_system.calls(), [verified_read_call(&path)]);
    }
}

#[tokio::test]
async fn executor_snapshot_keeps_remote_native_paths_on_retained_authority() {
    let path = remote_native_path();
    assert!(path.to_abs_path().is_err());
    let source = workflow_source("remote-native");
    let file_system = Arc::new(SyntheticFileSystem::new(source.clone()));
    let entry = workflow(path.clone());
    let registry = executor_registry(Arc::clone(&file_system), &path);

    assert_eq!(
        registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap(),
        WorkflowSourceSnapshot {
            metadata: entry,
            source: Arc::from(source),
        }
    );
    assert_eq!(file_system.calls(), [verified_read_call(&path)]);
}

#[tokio::test]
async fn executor_snapshot_drops_verified_stream_after_early_rejection() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let mut file_system = SyntheticFileSystem::new(Bytes::new());
    file_system.stream = vec![Ok(Bytes::new())];
    file_system.stream_drop_probe = Some(Arc::clone(&dropped));
    let file_system = Arc::new(file_system);

    executor_registry(Arc::clone(&file_system), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();

    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(file_system.calls(), [verified_read_call(&path)]);
}

#[tokio::test]
async fn cancelling_executor_snapshot_drops_pending_verified_stream() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let probe = Arc::new(PendingStreamProbe {
        polled: Notify::new(),
        dropped: AtomicBool::new(false),
    });
    let mut file_system = SyntheticFileSystem::new(Bytes::new());
    file_system.capture_size = 1;
    file_system.pending_stream_probe = Some(Arc::clone(&probe));
    let file_system = Arc::new(file_system);
    let registry = executor_registry(Arc::clone(&file_system), &path);

    let task = tokio::spawn(async move { registry.source_snapshot_by_name("review").await });
    probe.polled.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(probe.dropped.load(Ordering::SeqCst));
    assert_eq!(file_system.calls(), [verified_read_call(&path)]);
}
