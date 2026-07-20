use std::io;

use pretty_assertions::assert_eq;

#[cfg(unix)]
use super::CURSOR_FILE;
#[cfg(unix)]
use super::LOCK_FILE;
#[cfg(unix)]
use super::RECOVERY_SUBDIR;
use super::WorkflowRecoveryCursor;
use super::WorkflowRecoveryCursorInvalidContent;
use super::WorkflowRecoveryCursorRead;

#[test]
fn cursor_lock_is_exclusive_and_released_on_drop() {
    let home = tempfile::tempdir().expect("tempdir");
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor store");
    let guard = cursor
        .try_lock()
        .expect("first lock attempt")
        .expect("first lock should succeed");

    assert!(cursor.try_lock().expect("contended lock attempt").is_none());

    drop(guard);
    assert!(cursor.try_lock().expect("lock after guard drop").is_some());
}

#[test]
fn cursor_replace_read_and_reset_are_typed() {
    let home = tempfile::tempdir().expect("tempdir");
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor store");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor lock available");
    let run_id = uuid::Uuid::now_v7().to_string();

    assert_eq!(
        guard.read().expect("read missing cursor"),
        WorkflowRecoveryCursorRead::MissingOrEmpty
    );
    guard.replace(&run_id).expect("replace cursor");
    assert_eq!(
        guard.read().expect("read cursor"),
        WorkflowRecoveryCursorRead::Value(run_id)
    );
    guard.reset().expect("reset cursor");
    assert_eq!(
        guard.read().expect("read reset cursor"),
        WorkflowRecoveryCursorRead::MissingOrEmpty
    );
    guard.mark_complete().expect("mark cursor complete");
    assert_eq!(
        guard.read().expect("read complete cursor"),
        WorkflowRecoveryCursorRead::Complete
    );
    guard.invalidate().expect("invalidate cursor");
    assert_eq!(
        guard.read().expect("read invalidated cursor"),
        WorkflowRecoveryCursorRead::MissingOrEmpty
    );
}

#[test]
fn cursor_rejects_noncanonical_replacements() {
    let home = tempfile::tempdir().expect("tempdir");
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor store");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor lock available");

    let error = guard
        .replace("NOT-A-CANONICAL-UUID")
        .expect_err("reject cursor");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        guard.read().expect("read missing cursor"),
        WorkflowRecoveryCursorRead::MissingOrEmpty
    );
}

#[test]
fn cursor_rejects_a_relative_codex_home() {
    let error = WorkflowRecoveryCursor::open(std::path::Path::new("relative-codex-home"))
        .expect_err("relative Codex home must fail closed");

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn corrupt_cursor_content_is_non_authoritative() {
    let home = tempfile::tempdir().expect("tempdir");
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor store");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor lock available");

    std::fs::write(&guard.cursor_path, vec![b'x'; 37]).expect("write oversized cursor");
    assert_eq!(
        guard.read().expect("classify oversized cursor"),
        WorkflowRecoveryCursorRead::InvalidContent(WorkflowRecoveryCursorInvalidContent::Oversized)
    );

    std::fs::write(&guard.cursor_path, [0xff]).expect("write invalid UTF-8 cursor");
    assert_eq!(
        guard.read().expect("classify invalid UTF-8 cursor"),
        WorkflowRecoveryCursorRead::InvalidContent(
            WorkflowRecoveryCursorInvalidContent::InvalidUtf8
        )
    );

    std::fs::write(&guard.cursor_path, b"not-a-run-id").expect("write invalid run id cursor");
    assert_eq!(
        guard.read().expect("classify invalid run id cursor"),
        WorkflowRecoveryCursorRead::InvalidContent(
            WorkflowRecoveryCursorInvalidContent::NonCanonicalRunId
        )
    );
}

#[cfg(unix)]
#[test]
fn cursor_storage_is_private_and_symlinks_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("tempdir");
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor store");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor lock available");
    guard
        .replace(&uuid::Uuid::now_v7().to_string())
        .expect("write cursor");
    let recovery_root = home.path().join(RECOVERY_SUBDIR);

    assert_eq!(
        std::fs::metadata(&recovery_root)
            .expect("recovery directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for file_name in [CURSOR_FILE, LOCK_FILE] {
        assert_eq!(
            std::fs::metadata(recovery_root.join(file_name))
                .expect("recovery file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    drop(guard);
    let outside = home.path().join("outside");
    std::fs::write(&outside, b"keep").expect("write outside target");
    std::fs::remove_file(recovery_root.join(CURSOR_FILE)).expect("remove cursor");
    symlink(&outside, recovery_root.join(CURSOR_FILE)).expect("create cursor symlink");
    let guard = cursor
        .try_lock()
        .expect("relock cursor")
        .expect("cursor lock available");
    guard.read().expect_err("cursor symlink must fail");
    assert_eq!(
        std::fs::read(&outside).expect("read outside target"),
        b"keep"
    );
}
