use anyhow::Result;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LocalFileSystem;
use codex_exec_server::VerifiedFileReadOptions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn local_filesystem_returns_one_immutable_verified_value() -> Result<()> {
    let temp = TempDir::new()?;
    let path = temp.path().join("workflow.js");
    std::fs::write(&path, b"trusted")?;
    let file_system = LocalFileSystem::unsandboxed();
    let capture = file_system
        .read_file_verified(
            &PathUri::from_host_native_path(path.canonicalize()?)?,
            VerifiedFileReadOptions { max_bytes: 7 },
            /*sandbox*/ None,
        )
        .await?;

    std::fs::write(&path, b"hostile")?;
    assert_eq!(capture.size, 7);
    assert_eq!(
        capture.stream.try_collect::<Vec<_>>().await?,
        vec![bytes::Bytes::from_static(b"trusted")]
    );
    Ok(())
}

#[tokio::test]
async fn local_verified_read_rejects_platform_sandbox_but_accepts_disabled_context() -> Result<()> {
    let temp = TempDir::new()?;
    let path = temp.path().join("workflow.js");
    std::fs::write(&path, b"source")?;
    let uri = PathUri::from_host_native_path(path.canonicalize()?)?;
    let file_system = LocalFileSystem::unsandboxed();
    let sandbox = read_only_sandbox(temp.path().to_path_buf())?;

    let error = file_system
        .read_file_verified(
            &uri,
            VerifiedFileReadOptions { max_bytes: 6 },
            Some(&sandbox),
        )
        .await
        .err()
        .expect("platform sandbox should be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

    let disabled = FileSystemSandboxContext::from_permission_profile(PermissionProfile::Disabled);
    let capture = file_system
        .read_file_verified(
            &uri,
            VerifiedFileReadOptions { max_bytes: 6 },
            Some(&disabled),
        )
        .await?;
    assert_eq!(
        capture.stream.try_collect::<Vec<_>>().await?,
        vec![bytes::Bytes::from_static(b"source")]
    );
    Ok(())
}

fn read_only_sandbox(path: std::path::PathBuf) -> Result<FileSystemSandboxContext> {
    let path = AbsolutePathBuf::from_absolute_path(&path)?;
    Ok(FileSystemSandboxContext::from_permission_profile(
        PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
                path: FileSystemPath::Path { path },
                access: FileSystemAccessMode::Read,
            }]),
            NetworkSandboxPolicy::Restricted,
        ),
    ))
}
