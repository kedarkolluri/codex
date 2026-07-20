#[cfg(unix)]
use std::fs;

use pretty_assertions::assert_eq;

use super::WorkflowRunLease;
use super::WorkflowRunLeaseAcquire;
use crate::storage::WorkflowRunPaths;

#[test]
fn acquiring_an_existing_lease_never_creates_a_missing_marker() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run dir");

    assert!(
        WorkflowRunLease::try_acquire_existing(&paths)
            .expect("inspect missing lease")
            .is_none()
    );
    assert!(!paths.lease().exists());

    let WorkflowRunLeaseAcquire::Acquired(created) =
        WorkflowRunLease::try_acquire(&paths).expect("create lease marker")
    else {
        panic!("new lease should be acquired");
    };
    drop(created);

    assert!(matches!(
        WorkflowRunLease::try_acquire_existing(&paths).expect("acquire existing lease"),
        Some(WorkflowRunLeaseAcquire::Acquired(_))
    ));
}

#[test]
fn a_second_handle_cannot_take_a_live_lease_and_drop_releases_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run dir");

    let WorkflowRunLeaseAcquire::Acquired(first) =
        WorkflowRunLease::try_acquire(&paths).expect("first lease attempt")
    else {
        panic!("first lease should be acquired");
    };
    assert_eq!(first.path(), paths.lease());
    assert!(matches!(
        WorkflowRunLease::try_acquire(&paths).expect("contended lease attempt"),
        WorkflowRunLeaseAcquire::Held
    ));

    drop(first);
    assert!(matches!(
        WorkflowRunLease::try_acquire(&paths).expect("lease after owner drop"),
        WorkflowRunLeaseAcquire::Acquired(_)
    ));
}

#[cfg(unix)]
#[test]
fn lease_symlink_is_rejected_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run dir");
    let target = home.path().join("outside");
    fs::write(&target, b"keep").expect("write outside target");
    symlink(&target, paths.lease()).expect("create lease symlink");

    let error = WorkflowRunLease::try_acquire(&paths).expect_err("symlink must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&target).expect("read outside target"), b"keep");
}
