use crate::NullOrdinal;
use crate::PhaseLine;
use crate::WorkflowRunLease;
use crate::WorkflowRunLeaseAcquire;
use crate::WorkflowRunMeta;
use crate::WorkflowRunStatus;
use crate::canonical_value_hash;
use crate::recorder::JournalRecorder;
use crate::storage::WorkflowRunPaths;
use crate::storage::mint_run_id;
use pretty_assertions::assert_eq;
use serde_json::json;

#[cfg(unix)]
const PERMISSIVE_UMASK_CHILD: &str = "CODEX_WORKFLOW_PRIVATE_UMASK_CHILD";
#[cfg(unix)]
const PRIVATE_LAYOUT_TEST: &str = "private_fs::tests::run_layout_is_private_with_permissive_umask";

#[tokio::test]
async fn run_layout_is_private_with_permissive_umask() {
    #[cfg(unix)]
    if std::env::var_os(PERMISSIVE_UMASK_CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("current test exe"))
            .args(["--exact", PRIVATE_LAYOUT_TEST, "--nocapture"])
            .env(PERMISSIVE_UMASK_CHILD, "1")
            .output()
            .expect("run isolated permissive-umask test");
        assert!(
            output.status.success(),
            "isolated permission test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    #[cfg(unix)]
    // SAFETY: the Unix branch runs in a child test process selected with
    // `--exact`, so no other test or application thread observes this umask.
    let prior_umask = unsafe { libc::umask(0) };

    let home = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let args = json!({"secret": "do not expose"});
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        canonical_value_hash(&args),
        "private-run".to_string(),
        Some(100),
        1,
        "2026-07-19T00:00:00Z".to_string(),
    );

    paths
        .initialize("export default 'private';", &meta)
        .expect("initialize private run");
    paths
        .write_invocation_args(&args, &meta.args_hash)
        .expect("write private invocation");
    paths
        .write_progress_atomically(r#"{"state":"private"}"#)
        .expect("write private progress");
    paths
        .mark_execution_launched()
        .expect("write private launch marker");
    let lease = match WorkflowRunLease::try_acquire(&paths).expect("create private lease") {
        WorkflowRunLeaseAcquire::Acquired(lease) => lease,
        WorkflowRunLeaseAcquire::Held => panic!("fresh lease must be available"),
    };
    let recorder = JournalRecorder::new(&paths, &meta)
        .await
        .expect("create private journal");
    recorder
        .record_phase(PhaseLine {
            timestamp: None,
            ordinal: NullOrdinal,
            title: "private phase".to_string(),
        })
        .await
        .expect("durable append");
    recorder.shutdown().await.expect("durable shutdown");
    paths
        .update_status(WorkflowRunStatus::Completed)
        .expect("replace private metadata");
    drop(lease);

    for directory in [
        crate::storage::workflows_root(home.path()),
        crate::storage::runs_root(home.path()),
        paths.run_dir().to_path_buf(),
    ] {
        assert_private_directory(&directory);
    }
    for file in [
        paths.script(),
        paths.invocation(),
        paths.meta(),
        paths.progress(),
        paths.lease(),
        paths.launch_marker(),
        paths.journal(),
    ] {
        assert_private_file(&file);
    }
    assert_eq!(
        std::fs::read_to_string(paths.progress()).expect("read progress"),
        r#"{"state":"private"}"#
    );

    #[cfg(unix)]
    // SAFETY: restores the child process's original mask before the test exits.
    unsafe {
        libc::umask(prior_umask);
    }
}

#[cfg(unix)]
fn assert_private_directory(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        std::fs::metadata(path)
            .expect("private directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "{}",
        path.display()
    );
}

#[cfg(unix)]
fn assert_private_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    assert_eq!(
        std::fs::metadata(path)
            .expect("private file metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "{}",
        path.display()
    );
}

#[cfg(unix)]
#[test]
fn terminal_legacy_layout_is_hardened_on_read() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    std::fs::create_dir_all(paths.run_dir()).expect("create permissive legacy tree");
    for directory in [
        crate::storage::workflows_root(home.path()),
        crate::storage::runs_root(home.path()),
        paths.run_dir().to_path_buf(),
    ] {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775))
            .expect("make legacy directory permissive");
    }

    let args = json!({"legacy": true});
    let mut meta = WorkflowRunMeta::new(
        run_id,
        None,
        "blake3:script".to_string(),
        canonical_value_hash(&args),
        "legacy-terminal".to_string(),
        Some(100),
        1,
        "2026-07-19T00:00:00Z".to_string(),
    );
    meta.status = WorkflowRunStatus::Completed;
    let artifacts = vec![
        (paths.script(), b"export default null;".to_vec()),
        (
            paths.invocation(),
            crate::key::canonical_value_json(&args).into_bytes(),
        ),
        (
            paths.meta(),
            serde_json::to_string_pretty(&meta)
                .expect("serialize metadata")
                .into_bytes(),
        ),
        (paths.progress(), br#"{"state":"terminal"}"#.to_vec()),
        (paths.lease(), Vec::new()),
        (paths.launch_marker(), Vec::new()),
        (
            paths.journal(),
            format!(
                "{}\n",
                serde_json::to_string(&meta).expect("serialize journal metadata")
            )
            .into_bytes(),
        ),
    ];
    for (path, bytes) in artifacts {
        std::fs::write(&path, bytes).expect("write legacy artifact");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664))
            .expect("make legacy artifact permissive");
    }
    std::fs::set_permissions(paths.script(), std::fs::Permissions::from_mode(0o400))
        .expect("make one owner-readable legacy artifact read-only");

    assert_eq!(
        paths.read_meta_bounded().expect("read terminal metadata"),
        meta
    );
    for directory in [
        crate::storage::workflows_root(home.path()),
        crate::storage::runs_root(home.path()),
        paths.run_dir().to_path_buf(),
    ] {
        assert_private_directory(&directory);
    }
    for file in [
        paths.script(),
        paths.invocation(),
        paths.meta(),
        paths.progress(),
        paths.lease(),
        paths.launch_marker(),
        paths.journal(),
    ] {
        assert_private_file(&file);
    }
}

#[cfg(windows)]
fn assert_private_directory(path: &std::path::Path) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;

    let directory = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .expect("open private directory");
    crate::private_fs::ensure_private_permissions(&directory).expect("private directory DACL");
}

#[cfg(windows)]
fn assert_private_file(path: &std::path::Path) {
    let file = std::fs::File::open(path).expect("open private file");
    crate::private_fs::ensure_private_permissions(&file).expect("private file DACL");
}
