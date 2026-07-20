use std::io;

use pretty_assertions::assert_eq;

use super::read_named_workflow_source_bounded;
use super::read_workflow_source_bounded;

fn workflow_source(name: &str, marker: &str) -> String {
    format!("export const meta = {{ name: '{name}', description: 'test' }};\n// {marker}\n")
}

#[tokio::test]
async fn bounded_workflow_read_rejects_oversized_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let small = dir.path().join("small.js");
    tokio::fs::write(&small, workflow_source("small", "small"))
        .await
        .expect("write small");
    assert!(read_workflow_source_bounded(&small).await.is_ok());

    let big = dir.path().join("big.js");
    let body = vec![b'x'; (codex_core_workflows::WORKFLOW_SOURCE_MAX_BYTES as usize) + 1];
    tokio::fs::write(&big, &body).await.expect("write big");
    let error = read_workflow_source_bounded(&big)
        .await
        .expect_err("oversized file must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn named_workflow_read_accepts_the_exact_declared_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.js");
    let source = workflow_source("review", "original");
    tokio::fs::write(&path, &source)
        .await
        .expect("write workflow");

    assert_eq!(
        read_named_workflow_source_bounded(&path, "review")
            .await
            .expect("read matching workflow"),
        source
    );
}

#[tokio::test]
async fn named_workflow_read_rejects_a_different_declared_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.js");
    tokio::fs::write(&path, workflow_source("deploy", "different"))
        .await
        .expect("write workflow");

    let error = read_named_workflow_source_bounded(&path, "review")
        .await
        .expect_err("different declared name must reject");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "saved workflow source no longer declares the requested name"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn named_workflow_read_rejects_atomic_replacement_with_another_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.js");
    tokio::fs::write(&path, workflow_source("review", "discovered"))
        .await
        .expect("write discovered workflow");

    let replacement = dir.path().join("replacement.js");
    tokio::fs::write(&replacement, workflow_source("deploy", "replacement"))
        .await
        .expect("write replacement workflow");
    tokio::fs::rename(&replacement, &path)
        .await
        .expect("atomically replace workflow");

    let error = read_named_workflow_source_bounded(&path, "review")
        .await
        .expect_err("replacement must not retain stale registry authority");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "saved workflow source no longer declares the requested name"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_read_rejects_a_final_symlink_after_discovery() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.js");
    tokio::fs::write(&path, workflow_source("review", "discovered"))
        .await
        .expect("write discovered workflow");
    let referent = dir.path().join("referent.js");
    tokio::fs::write(&referent, workflow_source("review", "redirected"))
        .await
        .expect("write referent");
    tokio::fs::remove_file(&path)
        .await
        .expect("remove discovered workflow");
    symlink(&referent, &path).expect("replace workflow with symlink");

    read_named_workflow_source_bounded(&path, "review")
        .await
        .expect_err("final symlink must be rejected");
}
