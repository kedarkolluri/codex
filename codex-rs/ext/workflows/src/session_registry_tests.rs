use std::io;

use codex_file_system as fs;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;

use super::WorkflowSessionRegistryError;
use super::executor_project_root;

struct MetadataErrorFileSystem(io::ErrorKind);

impl MetadataErrorFileSystem {
    fn unexpected<'a, T: 'a>() -> fs::ExecutorFileSystemFuture<'a, T> {
        Box::pin(async { panic!("project-boundary discovery used an unexpected filesystem API") })
    }
}

impl fs::ExecutorFileSystem for MetadataErrorFileSystem {
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
        let kind = self.0;
        Box::pin(async move { Err(io::Error::new(kind, "metadata denied")) })
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

#[tokio::test]
async fn empty_markers_use_cwd_without_touching_the_filesystem() {
    let cwd = PathUri::parse("file:///workspace/project").expect("cwd URI");

    assert_eq!(
        executor_project_root(
            &MetadataErrorFileSystem(io::ErrorKind::PermissionDenied),
            &cwd,
            &[],
        )
        .await,
        Ok(cwd)
    );
}

#[tokio::test]
async fn missing_markers_use_cwd_but_other_metadata_errors_fail_closed() {
    let cwd = PathUri::parse("file:///workspace/project").expect("cwd URI");
    let markers = vec![".git".to_string()];

    assert_eq!(
        executor_project_root(
            &MetadataErrorFileSystem(io::ErrorKind::NotFound),
            &cwd,
            &markers,
        )
        .await,
        Ok(cwd.clone())
    );
    assert_eq!(
        executor_project_root(
            &MetadataErrorFileSystem(io::ErrorKind::PermissionDenied),
            &cwd,
            &markers,
        )
        .await,
        Err(WorkflowSessionRegistryError::ProjectBoundary {
            cwd,
            message: "metadata denied".to_string(),
        })
    );
}
