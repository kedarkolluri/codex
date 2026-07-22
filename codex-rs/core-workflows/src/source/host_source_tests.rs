use std::path::Path;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::OpenHostSource;
use super::read_host_source;
#[cfg(unix)]
use super::validate_host_path;

fn canonical_uri(path: &Path) -> PathUri {
    let path = AbsolutePathBuf::from_absolute_path_checked(path.canonicalize().unwrap()).unwrap();
    PathUri::from_abs_path(&path)
}

#[tokio::test]
async fn unchanged_opened_object_is_read_and_reverified() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let source = b"stable workflow source";
    std::fs::write(&path, source).unwrap();
    let uri = canonical_uri(&path);

    assert_eq!(read_host_source(&uri).await.unwrap(), source);
}

#[cfg(unix)]
#[tokio::test]
async fn restored_same_size_final_entry_substitution_is_rejected() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let original_path = temp.path().join("original.js");
    let original = b"trusted!";
    let substitute = b"hostile!";
    std::fs::write(&path, original).unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    validate_host_path(&uri, &native_path).await.unwrap();

    std::fs::rename(&path, &original_path).unwrap();
    std::fs::write(&path, substitute).unwrap();
    let mut opened = OpenHostSource::open(&uri, &native_path).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&original_path, &path).unwrap();

    assert_eq!(
        opened.read_stable(&uri).await.unwrap_err(),
        super::source_changed(&uri)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restored_ancestor_symlink_substitution_is_rejected_by_identity() {
    let temp = TempDir::new().unwrap();
    let selected = temp.path().join("selected");
    let original_directory = temp.path().join("original");
    let redirected = temp.path().join("redirected");
    std::fs::create_dir(&selected).unwrap();
    std::fs::create_dir(&redirected).unwrap();
    let path = selected.join("review.js");
    let original = b"trusted!";
    let substitute = b"hostile!";
    std::fs::write(&path, original).unwrap();
    std::fs::write(redirected.join("review.js"), substitute).unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    validate_host_path(&uri, &native_path).await.unwrap();

    std::fs::rename(&selected, &original_directory).unwrap();
    std::os::unix::fs::symlink(&redirected, &selected).unwrap();
    let mut opened = OpenHostSource::open(&uri, &native_path).await.unwrap();
    std::fs::remove_file(&selected).unwrap();
    std::fs::rename(&original_directory, &selected).unwrap();

    assert_eq!(opened.read_stable(&uri).await.unwrap(), substitute);
    assert_eq!(
        opened.verify_path(&uri, &native_path).await.unwrap_err(),
        super::source_changed(&uri)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn same_size_path_replacement_after_capture_is_rejected_by_identity() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    let original_path = temp.path().join("original.js");
    let original = b"trusted!";
    let substitute = b"hostile!";
    std::fs::write(&path, original).unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    let mut opened = OpenHostSource::open(&uri, &native_path).await.unwrap();
    assert_eq!(opened.read_stable(&uri).await.unwrap(), original);

    std::fs::rename(&path, &original_path).unwrap();
    std::fs::write(&path, substitute).unwrap();

    assert_eq!(
        opened.verify_path(&uri, &native_path).await.unwrap_err(),
        super::source_changed(&uri)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn same_length_in_place_mutation_with_restored_mtime_is_rejected() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    std::fs::write(&path, b"original").unwrap();
    let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    let mut opened = OpenHostSource::open(&uri, &native_path).await.unwrap();

    std::thread::sleep(std::time::Duration::from_millis(2));
    std::fs::write(&path, b"modified").unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();

    assert_eq!(
        opened.read_stable(&uri).await.unwrap_err(),
        super::source_changed(&uri)
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_opened_state_uses_handle_identity_and_denies_writers() {
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("review.js");
    std::fs::write(&path, b"source").unwrap();
    let uri = canonical_uri(&path);
    let native_path = uri.to_abs_path().unwrap();
    let opened = OpenHostSource::open(&uri, &native_path).await.unwrap();

    let error = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
    opened.verify_path(&uri, &native_path).await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_no_follow_open_keeps_file_symlink_as_reparse_point() {
    let temp = TempDir::new().unwrap();
    let referent = temp.path().join("referent.js");
    let link = temp.path().join("link.js");
    std::fs::write(&referent, "referent").unwrap();
    std::os::windows::fs::symlink_file(&referent, &link).unwrap();
    let uri =
        PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path_checked(link.clone()).unwrap());
    let native_path = uri.to_abs_path().unwrap();

    let Err(error) = OpenHostSource::open(&uri, &native_path).await else {
        panic!("expected a reparse-point rejection");
    };
    assert_eq!(error.path(), &uri);
}
