use std::path::Path;
use std::sync::Arc;

use codex_file_system as fs;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::WorkflowRootAssembly;
use crate::WorkflowRoot;
use crate::WorkflowScope;

struct UnusedFileSystem;

impl UnusedFileSystem {
    fn unexpected<'a, T: 'a>() -> fs::ExecutorFileSystemFuture<'a, T> {
        Box::pin(async { panic!("root assembly must not access the filesystem") })
    }
}

impl fs::ExecutorFileSystem for UnusedFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, PathUri> {
        Self::unexpected()
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<u8>> {
        Self::unexpected()
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileSystemReadStream> {
        Self::unexpected()
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unexpected()
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::CreateDirectoryOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unexpected()
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileMetadata> {
        Self::unexpected()
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<fs::ReadDirectoryEntry>> {
        Self::unexpected()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::RemoveOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unexpected()
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: fs::CopyOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        Self::unexpected()
    }
}

fn absolute(path: impl AsRef<Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute test path")
}

fn unused_file_system() -> Arc<dyn fs::ExecutorFileSystem> {
    Arc::new(UnusedFileSystem)
}

#[test]
fn host_roots_are_assembled_in_precedence_order() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    let user_home = absolute(temp.path().join("home"));
    let codex_home = absolute(temp.path().join("codex-home"));

    let roots = WorkflowRootAssembly::new(codex_home.clone())
        .with_user_home(user_home.clone())
        .with_host_project(project.clone())
        .into_roots();

    assert_eq!(
        roots,
        vec![
            WorkflowRoot::new(
                project.join(".codex").join("workflows"),
                WorkflowScope::Project,
            ),
            WorkflowRoot::new(
                user_home.join(".agents").join("workflows"),
                WorkflowScope::Personal,
            ),
            WorkflowRoot::new(codex_home.join("workflows"), WorkflowScope::CodexHome),
        ]
    );
}

#[test]
fn missing_optional_roots_leave_codex_home_only() {
    let temp = TempDir::new().expect("temp dir");
    let codex_home = absolute(temp.path().join("missing-codex-home"));

    assert_eq!(
        WorkflowRootAssembly::new(codex_home.clone()).into_roots(),
        vec![WorkflowRoot::new(
            codex_home.join("workflows"),
            WorkflowScope::CodexHome,
        )]
    );
}

#[test]
fn executor_project_join_is_cross_platform_and_retains_authority() {
    let temp = TempDir::new().expect("temp dir");
    let codex_home = absolute(temp.path().join("codex-home"));

    for (project_root, expected_root) in [
        (
            "file:///workspace/project",
            "file:///workspace/project/.codex/workflows",
        ),
        (
            "file:///Users/example/project",
            "file:///Users/example/project/.codex/workflows",
        ),
        (
            "file:///C:/workspace/project",
            "file:///C:/workspace/project/.codex/workflows",
        ),
        (
            "file://server/share/project",
            "file://server/share/project/.codex/workflows",
        ),
    ] {
        let project_root = PathUri::parse(project_root).expect("project root URI");
        let expected_root = PathUri::parse(expected_root).expect("workflow root URI");
        let file_system = unused_file_system();
        let roots = WorkflowRootAssembly::new(codex_home.clone())
            .with_executor_project(&project_root, Arc::clone(&file_system))
            .expect("join executor project root")
            .into_roots();

        assert_eq!(
            roots,
            vec![
                WorkflowRoot::project_on_executor(expected_root, file_system),
                WorkflowRoot::new(codex_home.join("workflows"), WorkflowScope::CodexHome,),
            ],
            "project root {project_root}"
        );
    }
}

#[test]
fn host_convertible_project_stays_on_executor() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    let codex_home = absolute(temp.path().join("codex-home"));
    let project_uri = PathUri::from_abs_path(&project);
    let expected_root = PathUri::from_abs_path(&project.join(".codex").join("workflows"));
    let file_system = unused_file_system();

    let roots = WorkflowRootAssembly::new(codex_home.clone())
        .with_executor_project(&project_uri, Arc::clone(&file_system))
        .expect("join native executor project root")
        .into_roots();

    assert_eq!(
        roots,
        vec![
            WorkflowRoot::project_on_executor(expected_root, file_system),
            WorkflowRoot::new(codex_home.join("workflows"), WorkflowScope::CodexHome),
        ]
    );
}

#[test]
fn identical_host_and_executor_uris_are_not_deduplicated() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    let codex_home = project.join(".codex");
    let shared_root = PathUri::from_abs_path(&codex_home.join("workflows"));
    let file_system = unused_file_system();

    let roots = WorkflowRootAssembly::new(codex_home.clone())
        .with_executor_project(&PathUri::from_abs_path(&project), Arc::clone(&file_system))
        .expect("join executor project root")
        .into_roots();

    assert_eq!(
        roots,
        vec![
            WorkflowRoot::project_on_executor(shared_root, file_system),
            WorkflowRoot::new(codex_home.join("workflows"), WorkflowScope::CodexHome),
        ]
    );
}

#[test]
fn opaque_executor_project_fails_closed() {
    let temp = TempDir::new().expect("temp dir");
    let codex_home = absolute(temp.path().join("codex-home"));
    let opaque = PathUri::parse("file:///%00/bad/path/L3Byb2plY3Q").expect("opaque path URI");
    let expected_error = opaque
        .join(".codex/workflows")
        .expect_err("opaque path must reject descendants");

    let result =
        WorkflowRootAssembly::new(codex_home).with_executor_project(&opaque, unused_file_system());

    assert_eq!(result, Err(expected_error));
}
