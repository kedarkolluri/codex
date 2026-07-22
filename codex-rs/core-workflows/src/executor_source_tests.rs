use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use codex_file_system as fs;
use codex_utils_path_uri::PathUri;
use futures::stream;
use pretty_assertions::assert_eq;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileSystemCall {
    Metadata,
    Canonicalize,
    Read,
}

const FULL_VALIDATION: [FileSystemCall; 5] = [
    FileSystemCall::Metadata,
    FileSystemCall::Canonicalize,
    FileSystemCall::Read,
    FileSystemCall::Metadata,
    FileSystemCall::Canonicalize,
];
const READ_STARTED: [FileSystemCall; 3] = [
    FileSystemCall::Metadata,
    FileSystemCall::Canonicalize,
    FileSystemCall::Read,
];

#[derive(Clone)]
struct FileState {
    metadata: fs::FileMetadata,
    canonical: PathUri,
}

fn file_state(path: &PathUri, size: u64) -> FileState {
    FileState {
        metadata: fs::FileMetadata {
            is_directory: false,
            is_file: true,
            is_symlink: false,
            size,
            created_at_ms: 1,
            modified_at_ms: 1,
        },
        canonical: path.clone(),
    }
}

struct SyntheticFileSystem {
    states: [FileState; 2],
    stream_open_error: Option<io::ErrorKind>,
    stream: Vec<Result<Bytes, io::ErrorKind>>,
    calls: Mutex<Vec<FileSystemCall>>,
}

impl SyntheticFileSystem {
    fn new(path: &PathUri, source: impl Into<Bytes>) -> Self {
        let source = source.into();
        let state = file_state(path, source.len() as u64);
        Self {
            states: [state.clone(), state],
            stream_open_error: None,
            stream: vec![Ok(source)],
            calls: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, call: FileSystemCall) -> usize {
        let mut calls = self.calls.lock().unwrap();
        let index = calls.iter().filter(|candidate| **candidate == call).count();
        assert!(index < 2, "unexpected repeated {call:?} call");
        calls.push(call);
        index
    }

    fn calls(&self) -> Vec<FileSystemCall> {
        self.calls.lock().unwrap().clone()
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
            let index = self.record(FileSystemCall::Canonicalize);
            Ok(self.states[index].canonical.clone())
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
            if let Some(kind) = self.stream_open_error {
                return Err(io::Error::new(kind, "source-secret"));
            }
            let chunks = self
                .stream
                .clone()
                .into_iter()
                .map(|chunk| chunk.map_err(|kind| io::Error::new(kind, "source-secret")));
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
            let index = self.record(FileSystemCall::Metadata);
            Ok(self.states[index].metadata.clone())
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

fn rejected_states(path: &PathUri, size: u64) -> Vec<(&'static str, FileState, String, bool)> {
    let initial = file_state(path, size);
    let mut symlink = initial.clone();
    symlink.metadata.is_symlink = true;
    let mut non_file = initial.clone();
    non_file.metadata.is_file = false;
    non_file.metadata.is_directory = true;
    let mut oversized = initial.clone();
    oversized.metadata.size = WORKFLOW_SOURCE_MAX_BYTES as u64 + 1;
    let mut canonical = initial;
    let outside = PathUri::parse("file:///outside/review.js").unwrap();
    canonical.canonical = outside.clone();
    vec![
        (
            "symlink",
            symlink,
            "workflow source is a symlink".to_string(),
            false,
        ),
        (
            "non-file",
            non_file,
            "workflow source is not a regular file".to_string(),
            false,
        ),
        (
            "oversize",
            oversized,
            format!("workflow source exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"),
            false,
        ),
        (
            "canonical",
            canonical,
            format!("workflow source now resolves to {outside}"),
            true,
        ),
    ]
}

#[tokio::test]
async fn executor_snapshot_uses_winning_authority_and_accepts_exact_limit() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source_a = workflow_source("authority-a");
    let mut source_b = workflow_source("authority-b");
    source_b.push_str(&" ".repeat(WORKFLOW_SOURCE_MAX_BYTES - source_b.len()));
    let file_system_a = Arc::new(SyntheticFileSystem::new(&path, source_a));
    let file_system_b = Arc::new(SyntheticFileSystem::new(&path, source_b.clone()));
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
    assert_eq!(file_system_b.calls(), FULL_VALIDATION);
}

#[tokio::test]
async fn executor_snapshot_rejects_invalid_stream_shapes() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let mut oversized = SyntheticFileSystem::new(&path, Bytes::new());
    oversized.stream = vec![
        Ok(Bytes::from(vec![b' '; WORKFLOW_SOURCE_MAX_BYTES])),
        Ok(Bytes::from_static(b"x")),
    ];
    let oversized = Arc::new(oversized);
    let error = executor_registry(Arc::clone(&oversized), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_load_error(
        error,
        &path,
        &format!("workflow source exceeds the {WORKFLOW_SOURCE_MAX_BYTES}-byte limit"),
    );
    assert_eq!(oversized.calls(), READ_STARTED);

    let mut empty = SyntheticFileSystem::new(&path, Bytes::new());
    empty.stream = vec![Ok(Bytes::new())];
    let empty = Arc::new(empty);
    let error = executor_registry(Arc::clone(&empty), &path)
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_load_error(error, &path, "source stream returned an empty chunk");
    assert_eq!(empty.calls(), READ_STARTED);

    let source = workflow_source("length-mismatch");
    let mismatches = [source[..source.len() - 1].to_string(), format!("{source}x")];
    for stream in mismatches {
        let mut file_system = SyntheticFileSystem::new(&path, source.clone());
        file_system.stream = vec![Ok(Bytes::from(stream))];
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, "workflow source changed while being read");
        assert_eq!(file_system.calls(), FULL_VALIDATION);
    }
}

#[tokio::test]
async fn executor_snapshot_rejects_stream_io_failures_without_post_validation() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source = workflow_source("io-error");
    let cases = [
        (
            Some(io::ErrorKind::PermissionDenied),
            vec![Ok(Bytes::from(source.clone()))],
            "failed to read source (PermissionDenied)",
        ),
        (
            None,
            vec![
                Ok(Bytes::from_static(b"partial")),
                Err(io::ErrorKind::ConnectionReset),
            ],
            "failed to read source (ConnectionReset)",
        ),
    ];
    for (stream_open_error, stream, message) in cases {
        let mut file_system = SyntheticFileSystem::new(&path, source.clone());
        file_system.stream_open_error = stream_open_error;
        file_system.stream = stream;
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, message);
        assert_eq!(file_system.calls(), READ_STARTED);
    }
}

#[tokio::test]
async fn executor_snapshot_rejects_each_invalid_initial_state_before_reading() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source = Bytes::from(workflow_source("stable"));
    for (name, state, message, reaches_canonicalize) in rejected_states(&path, source.len() as u64)
    {
        let mut file_system = SyntheticFileSystem::new(&path, source.clone());
        file_system.states[0] = state;
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, &message);
        let call_count = if reaches_canonicalize { 2 } else { 1 };
        assert_eq!(file_system.calls(), READ_STARTED[..call_count], "{name}");
    }
}

#[tokio::test]
async fn executor_snapshot_rejects_each_observed_post_read_drift() {
    let path = PathUri::parse("file:///remote/workflows/review.js").unwrap();
    let source = Bytes::from(workflow_source("stable"));
    let initial = file_state(&path, source.len() as u64);
    let mut modified = initial.clone();
    modified.metadata.modified_at_ms += 1;
    let mut created = initial.clone();
    created.metadata.created_at_ms += 1;
    let mut resized = initial;
    resized.metadata.size += 1;
    let mut extended = source.to_vec();
    extended.push(b'x');
    let metadata_cases = [
        ("modified", modified, source.clone()),
        ("created", created, source.clone()),
        ("size", resized, Bytes::from(extended)),
    ];
    for (name, after, stream) in metadata_cases {
        let mut file_system = SyntheticFileSystem::new(&path, source.clone());
        file_system.states[1] = after;
        file_system.stream = vec![Ok(stream)];
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, "workflow source changed while being read");
        assert_eq!(file_system.calls(), FULL_VALIDATION, "{name}");
    }

    for (name, after, message, reaches_canonicalize) in rejected_states(&path, source.len() as u64)
    {
        let mut file_system = SyntheticFileSystem::new(&path, source.clone());
        file_system.states[1] = after;
        let file_system = Arc::new(file_system);
        let error = executor_registry(Arc::clone(&file_system), &path)
            .source_snapshot_by_name("review")
            .await
            .unwrap_err();
        assert_load_error(error, &path, &message);
        let call_count = if reaches_canonicalize { 5 } else { 4 };
        assert_eq!(file_system.calls(), FULL_VALIDATION[..call_count], "{name}");
    }
}
