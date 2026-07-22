use std::path::Path;

use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::MAX_VERIFIED_FILE_CAPTURE_BYTES;
use super::OpenVerifiedFile;
use super::capture;

fn canonical_uri(path: &Path) -> PathUri {
    PathUri::from_host_native_path(path.canonicalize().unwrap()).unwrap()
}

#[tokio::test]
async fn captures_stable_empty_exact_limit_and_rejects_oversize() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("workflow.js");

    std::fs::write(&path, []).unwrap();
    let uri = canonical_uri(&path);
    assert_eq!(capture(&uri, /*max_bytes*/ 0).await.unwrap(), b""[..]);

    std::fs::write(&path, b"stable").unwrap();
    assert_eq!(capture(&uri, /*max_bytes*/ 6).await.unwrap(), b"stable"[..]);
    let error = capture(&uri, /*max_bytes*/ 5).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "verified file exceeds the 5-byte limit");

    std::fs::File::create(&path)
        .unwrap()
        .set_len(MAX_VERIFIED_FILE_CAPTURE_BYTES + 1)
        .unwrap();
    let error = capture(&uri, /*max_bytes*/ u64::MAX).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        format!("verified file exceeds the {MAX_VERIFIED_FILE_CAPTURE_BYTES}-byte limit")
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn rejects_final_symlink_or_reparse_point() {
    let temp = TempDir::new().unwrap();
    let referent = temp.path().join("referent.js");
    let link = temp.path().join("link.js");
    std::fs::write(&referent, "referent").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&referent, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&referent, &link).unwrap();
    let uri = PathUri::from_host_native_path(&link).unwrap();

    let error = capture(&uri, /*max_bytes*/ 64).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "verified file is not a regular, non-symlink file"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_noncanonical_ancestor_symlink_alias() {
    let temp = TempDir::new().unwrap();
    let actual = temp.path().join("actual");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&actual).unwrap();
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let path = alias.join("workflow.js");
    std::fs::write(actual.join("workflow.js"), b"source").unwrap();
    let uri = PathUri::from_host_native_path(&path).unwrap();

    let error = capture(&uri, /*max_bytes*/ 6).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .starts_with("verified file path resolves to ")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_same_size_path_replacement_by_identity() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("workflow.js");
    let original_path = temp.path().join("original.js");
    std::fs::write(&path, b"trusted!").unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    let mut opened = OpenVerifiedFile::open(&native_path, /*max_bytes*/ 8)
        .await
        .unwrap();
    assert_eq!(
        opened.read_stable(/*max_bytes*/ 8).await.unwrap(),
        b"trusted!"
    );

    std::fs::rename(&path, &original_path).unwrap();
    std::fs::write(&path, b"hostile!").unwrap();

    let error = opened
        .verify_path(&uri, &native_path, /*max_bytes*/ 8)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "verified file changed while being captured"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restored_mtime_does_not_hide_in_place_mutation() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("workflow.js");
    std::fs::write(&path, b"original").unwrap();
    let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    let mut opened = OpenVerifiedFile::open(&native_path, /*max_bytes*/ 8)
        .await
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(2));
    std::fs::write(&path, b"modified").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();

    let error = opened.read_stable(/*max_bytes*/ 8).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[tokio::test]
async fn no_follow_open_rejects_fifo_without_waiting_for_a_writer() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("named-pipe");
    let output = std::process::Command::new("mkfifo")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let uri = canonical_uri(&path);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        capture(&uri, /*max_bytes*/ 64),
    )
    .await
    .expect("opening a FIFO should not wait for a writer");
    let Err(error) = result else {
        panic!("opening a FIFO should fail");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(windows)]
#[tokio::test]
async fn no_follow_open_rejects_named_pipe_without_hanging() {
    use tokio::net::windows::named_pipe::ServerOptions;
    use uuid::Uuid;

    let pipe_path = format!(r"\\.\pipe\codex-verified-read-{}", Uuid::new_v4());
    let _pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_path)
        .unwrap();
    let uri = PathUri::from_host_native_path(Path::new(&pipe_path)).unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        capture(&uri, /*max_bytes*/ 64),
    )
    .await
    .expect("opening a named pipe should not hang");
    let Err(error) = result else {
        panic!("opening a named pipe should fail");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(windows)]
#[tokio::test]
async fn open_denies_writers_while_verifying() {
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("workflow.js");
    std::fs::write(&path, b"source").unwrap();
    let uri = canonical_uri(&path);
    let opened = OpenVerifiedFile::open(&uri.to_abs_path().unwrap(), /*max_bytes*/ 6)
        .await
        .unwrap();

    let error = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
    drop(opened);
}
