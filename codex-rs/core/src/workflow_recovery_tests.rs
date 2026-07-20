use std::fs;

use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::WorkflowRecoveryCursor;
use codex_workflow_journal::WorkflowRecoveryCursorRead;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::storage::MAX_META_FILE_BYTES;
use codex_workflow_journal::storage::WorkflowRunPaths;
use pretty_assertions::assert_eq;

use super::reconcile_stale_workflow_run;
use super::reconcile_stale_workflow_runs;
use crate::tools::code_mode::workflow_progress::durable::DurableProgressRead;
use crate::tools::code_mode::workflow_progress::durable::DurableRunState;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_progress::durable::read;
use crate::tools::code_mode::workflow_progress::durable::record_event;
use crate::tools::code_mode::workflow_progress::durable::record_terminal_event;

const OWNER_THREAD_ID: &str = "01900000-0000-7000-8000-000000000001";

fn sample_meta(run_id: &str, name: &str) -> WorkflowRunMeta {
    WorkflowRunMeta::new(
        run_id.to_string(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        name.to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    )
    .with_owner_thread_id(OWNER_THREAD_ID.to_string())
}

fn initialize_lease_era(home: &std::path::Path, run_id: &str, name: &str) -> WorkflowRunPaths {
    let paths = WorkflowRunPaths::new(home, run_id);
    paths.create_dir().expect("create run directory");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("create lease marker")
    else {
        panic!("new run should acquire its lease");
    };
    paths
        .initialize("export default null;", &sample_meta(run_id, name))
        .expect("initialize run");
    drop(lease);
    paths
}

async fn record_begin(home: &std::path::Path, run_id: &str, name: &str) {
    record_event(
        home,
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.to_string(),
            resumed_from_run_id: None,
            name: name.to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
    )
    .await
    .expect("record run begin");
}

async fn complete_empty_index(home: &std::path::Path, state: &codex_state::StateRuntime) {
    let report = reconcile_stale_workflow_runs(home, Some(state)).await;
    assert_eq!(report.scanned, 0);
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("read completed index state"),
        codex_state::WorkflowRunFilesystemIndexState::Complete
    );
    let cursor = WorkflowRecoveryCursor::open(home).expect("open recovery cursor");
    let guard = cursor
        .try_lock()
        .expect("lock recovery cursor")
        .expect("recovery cursor is idle");
    assert_eq!(
        guard.read().expect("read recovery cursor"),
        WorkflowRecoveryCursorRead::Complete
    );
}

#[tokio::test]
async fn live_lease_is_never_reconciled_by_a_second_handle() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run directory");
    let WorkflowRunLeaseAcquire::Acquired(_lease) =
        WorkflowRunLease::try_acquire(&paths).expect("acquire live lease")
    else {
        panic!("live run should acquire its lease");
    };
    paths
        .initialize("export default null;", &sample_meta(&run_id, "live"))
        .expect("initialize live run");

    let report = reconcile_stale_workflow_run(home.path(), &run_id, None).await;

    assert_eq!(report.active, 1);
    assert_eq!(report.reconciled, 0);
    assert!(report.diagnostics.is_empty());
    assert_eq!(
        paths.read_meta_bounded().expect("read live meta").status,
        WorkflowRunStatus::Running
    );
}

#[tokio::test]
async fn dropped_lease_is_reconciled_once_to_interrupted() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run directory");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("acquire live lease")
    else {
        panic!("run should acquire its lease");
    };
    paths
        .initialize("export default null;", &sample_meta(&run_id, "crashed"))
        .expect("initialize run");
    record_begin(home.path(), &run_id, "crashed").await;
    drop(lease);
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "crashed".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Running,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        })
        .await
        .expect("seed running projection");

    let first = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;
    assert_eq!(first.reconciled, 1);
    assert_eq!(first.reconciled_run_ids, vec![run_id.clone()]);
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read recovered meta")
            .status,
        WorkflowRunStatus::Failed
    );
    let DurableProgressRead::Snapshot(snapshot) =
        read(home.path(), &run_id).await.expect("read progress")
    else {
        panic!("expected recovered progress snapshot");
    };
    assert_eq!(snapshot.state, DurableRunState::Terminal);
    assert_eq!(snapshot.status, DurableRunStatus::Interrupted);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read state projection")
            .expect("workflow projection exists"),
        codex_state::WorkflowRun {
            run_id: run_id.clone(),
            name: "crashed".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Failed,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        }
    );

    let second = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;
    assert_eq!(second.reconciled, 0);
    assert_eq!(second.already_terminal, 1);
    state.close().await;
}

#[tokio::test]
async fn head_format_run_without_lease_is_byte_exact_and_projects_unknown_safely() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create legacy run directory");
    let created_at = "2026-07-17T00:00:00Z";
    let legacy_meta = serde_json::json!({
        "type": "run_meta",
        "run_id": run_id.clone(),
        "parent_run_id": null,
        "script_hash": "blake3:script",
        "args_hash": "blake3:args",
        "name": "legacy-completed",
        "budget_total": 1_000,
        "key_algo_version": 1,
        "created_at": created_at,
    });
    let meta_bytes = serde_json::to_vec_pretty(&legacy_meta).expect("serialize legacy metadata");
    let mut journal_bytes = serde_json::to_vec(&legacy_meta).expect("serialize legacy journal");
    journal_bytes.push(b'\n');
    fs::write(paths.script(), b"export default null;").expect("write legacy script");
    fs::write(paths.meta(), &meta_bytes).expect("write legacy metadata");
    fs::write(paths.journal(), &journal_bytes).expect("write legacy journal");
    assert!(!paths.lease().exists());
    assert!(!paths.progress().exists());

    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "legacy-completed".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: None,
            status: codex_state::WorkflowRunStatus::Completed,
            created_at: created_at.to_string(),
        })
        .await
        .expect("seed completed legacy projection");

    let report = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;

    assert_eq!(report.scanned, 1);
    assert_eq!(report.reconciled, 0);
    assert_eq!(report.active, 0);
    assert_eq!(report.already_terminal, 0);
    assert_eq!(report.legacy_unknown, 1);
    assert_eq!(report.legacy_unknown_run_ids, vec![run_id.clone()]);
    assert!(report.diagnostics.is_empty());
    assert_eq!(fs::read(paths.meta()).expect("read metadata"), meta_bytes);
    assert_eq!(
        fs::read(paths.journal()).expect("read journal"),
        journal_bytes
    );
    assert!(!paths.lease().exists());
    assert!(!paths.progress().exists());
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read legacy projection")
            .expect("legacy projection exists")
            .status,
        codex_state::WorkflowRunStatus::Completed
    );

    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "legacy-completed".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: None,
            status: codex_state::WorkflowRunStatus::Running,
            created_at: created_at.to_string(),
        })
        .await
        .expect("replace terminal evidence with a stale running projection");
    let reprojected =
        reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;
    assert_eq!(reprojected.legacy_unknown, 1);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read reprojected legacy run")
            .expect("legacy projection exists"),
        codex_state::WorkflowRun {
            run_id: run_id.clone(),
            name: "legacy-completed".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: None,
            status: codex_state::WorkflowRunStatus::Unknown,
            created_at: created_at.to_string(),
        }
    );
    assert_eq!(fs::read(paths.meta()).expect("read metadata"), meta_bytes);
    assert_eq!(
        fs::read(paths.journal()).expect("read journal"),
        journal_bytes
    );
    state.close().await;
}

#[tokio::test]
async fn scan_skips_damaged_runs_and_continues_reconciling() {
    let home = tempfile::tempdir().expect("tempdir");
    let valid_id = uuid::Uuid::now_v7().to_string();
    let valid = initialize_lease_era(home.path(), &valid_id, "valid");
    fs::write(valid.progress(), b"{broken").expect("write corrupt progress");

    let corrupt_meta_id = uuid::Uuid::now_v7().to_string();
    let corrupt_meta = WorkflowRunPaths::new(home.path(), &corrupt_meta_id);
    corrupt_meta.create_dir().expect("create corrupt run");
    fs::write(corrupt_meta.meta(), b"{broken").expect("write corrupt meta");

    let over_cap_id = uuid::Uuid::now_v7().to_string();
    let over_cap = WorkflowRunPaths::new(home.path(), &over_cap_id);
    over_cap.create_dir().expect("create over-cap run");
    fs::write(
        over_cap.meta(),
        vec![b'x'; MAX_META_FILE_BYTES as usize + 1],
    )
    .expect("write over-cap meta");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let symlink_id = uuid::Uuid::now_v7().to_string();
        let outside = home.path().join("outside-run");
        fs::create_dir(&outside).expect("create outside dir");
        symlink(
            &outside,
            codex_workflow_journal::storage::runs_root(home.path()).join(symlink_id),
        )
        .expect("create run symlink");
    }

    let report = reconcile_stale_workflow_runs(home.path(), None).await;

    assert_eq!(report.reconciled, 1);
    assert_eq!(
        valid.read_meta_bounded().expect("read valid meta").status,
        WorkflowRunStatus::Failed
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|line| line.contains("corrupt progress"))
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|line| line.contains("metadata is unavailable"))
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|line| line.contains("byte cap"))
    );
    #[cfg(unix)]
    assert!(
        report
            .diagnostics
            .iter()
            .any(|line| line.contains("non-symlink directory"))
    );
}

#[tokio::test]
async fn healthy_complete_index_skips_the_run_directory() {
    let home = tempfile::tempdir().expect("tempdir");
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    complete_empty_index(home.path(), state.as_ref()).await;

    let runs_root = codex_workflow_journal::storage::runs_root(home.path());
    fs::remove_dir(&runs_root).expect("remove empty runs root");
    fs::write(&runs_root, b"not a directory").expect("replace runs root with a file");

    let report = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(report.scanned, 0);
    assert!(report.diagnostics.is_empty());
    state.close().await;
}

#[tokio::test]
async fn held_run_is_projected_before_backfill_completes_and_recovers_after_owner_exit() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create held run directory");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("acquire live lease")
    else {
        panic!("new run lease should be available")
    };
    paths
        .initialize("export default null;", &sample_meta(&run_id, "held"))
        .expect("initialize held run");
    record_begin(home.path(), &run_id, "held").await;
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");

    let first = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(first.active, 1);
    assert_eq!(first.reconciled, 0);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read held projection")
            .expect("held run must be indexed")
            .status,
        codex_state::WorkflowRunStatus::Running
    );
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("read filesystem index state"),
        codex_state::WorkflowRunFilesystemIndexState::Complete
    );

    drop(lease);
    let second = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(second.reconciled_run_ids, vec![run_id.clone()]);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read recovered projection")
            .expect("recovered run remains indexed")
            .status,
        codex_state::WorkflowRunStatus::Failed
    );
    state.close().await;
}

#[tokio::test]
async fn crashed_pending_publications_reopen_complete_index_without_leaving_ghost_rows() {
    let home = tempfile::tempdir().expect("tempdir");
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    complete_empty_index(home.path(), state.as_ref()).await;

    let absent_id = uuid::Uuid::now_v7().to_string();
    let absent_meta = sample_meta(&absent_id, "absent");
    let absent_params = super::run_index_params(home.path(), &absent_meta);
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open recovery cursor");
    let guard = cursor
        .try_lock()
        .expect("lock recovery cursor")
        .expect("cursor is idle");
    assert_eq!(
        state
            .begin_workflow_run_publication(&absent_params)
            .await
            .expect("reserve absent publication"),
        codex_state::WorkflowRunPublicationAdmission::InsertedPending
    );
    drop(guard);

    let absent = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(absent.scanned, 0);
    assert_eq!(state.get_workflow_run(&absent_id).await.unwrap(), None);
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("absent ghost cleanup completes"),
        codex_state::WorkflowRunFilesystemIndexState::Complete
    );

    let artifact_id = uuid::Uuid::now_v7().to_string();
    let artifact_meta = sample_meta(&artifact_id, "artifact-crash");
    let artifact_params = super::run_index_params(home.path(), &artifact_meta);
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("reopen recovery cursor");
    let guard = cursor
        .try_lock()
        .expect("lock recovery cursor")
        .expect("cursor is idle");
    state
        .begin_workflow_run_publication(&artifact_params)
        .await
        .expect("reserve artifact publication");
    let paths = WorkflowRunPaths::new(home.path(), &artifact_id);
    paths.create_dir().expect("create artifact run directory");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("acquire artifact lease")
    else {
        panic!("artifact fixture lease should be available")
    };
    paths
        .initialize("export default null;", &artifact_meta)
        .expect("publish exact artifacts");
    codex_workflow_journal::JournalRecorder::new(&paths, &artifact_meta)
        .await
        .expect("publish exact journal")
        .shutdown()
        .await
        .expect("close exact journal");
    record_begin(home.path(), &artifact_id, "artifact-crash").await;
    drop(lease);
    drop(guard);

    let artifact = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(artifact.reconciled_run_ids, vec![artifact_id.clone()]);
    assert_eq!(
        state
            .get_workflow_run(&artifact_id)
            .await
            .expect("read artifact projection")
            .expect("partial publication was backfilled")
            .status,
        codex_state::WorkflowRunStatus::Failed
    );
    state.close().await;
}

#[tokio::test]
async fn live_pending_publisher_is_not_cleaned_while_it_holds_the_shared_lock() {
    let home = tempfile::tempdir().expect("tempdir");
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    complete_empty_index(home.path(), state.as_ref()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let meta = sample_meta(&run_id, "live-publisher");
    let params = super::run_index_params(home.path(), &meta);
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open recovery cursor");
    let guard = cursor
        .try_lock()
        .expect("lock publication cursor")
        .expect("cursor is idle");
    state
        .begin_workflow_run_publication(&params)
        .await
        .expect("reserve live publication");

    let concurrent = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(concurrent.scanned, 0);
    assert!(
        concurrent
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("active in another process"))
    );
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths
        .create_dir()
        .expect("create live publication directory");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("acquire live publication lease")
    else {
        panic!("live publication lease should be available")
    };
    paths
        .initialize("export default null;", &meta)
        .expect("publish live artifacts");
    codex_workflow_journal::JournalRecorder::new(&paths, &meta)
        .await
        .expect("publish live journal")
        .shutdown()
        .await
        .expect("close live journal");
    record_begin(home.path(), &run_id, "live-publisher").await;
    assert!(
        state
            .commit_workflow_run_publication(&run_id)
            .await
            .expect("live pending row was not deleted")
    );
    drop(guard);
    assert!(state.get_workflow_run(&run_id).await.unwrap().is_some());
    drop(lease);
    state.close().await;
}

#[tokio::test]
async fn corrupt_directory_keeps_backfill_incomplete_until_repaired() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create corrupt run directory");
    fs::write(paths.meta(), b"{broken").expect("write corrupt metadata");
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");

    let corrupt = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert!(
        corrupt
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("metadata is unavailable"))
    );
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("corrupt cycle remains incomplete"),
        codex_state::WorkflowRunFilesystemIndexState::ContinueCursor
    );

    let repaired = initialize_lease_era(home.path(), &run_id, "repaired");
    record_begin(home.path(), &run_id, "repaired").await;
    let recovered = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(recovered.reconciled_run_ids, vec![run_id.clone()]);
    assert_eq!(
        repaired
            .read_meta_bounded()
            .expect("read repaired metadata")
            .status,
        WorkflowRunStatus::Failed
    );
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("repaired cycle completes"),
        codex_state::WorkflowRunFilesystemIndexState::Complete
    );
    state.close().await;
}

#[tokio::test]
async fn invalid_cursor_content_resets_safely_and_continues_recovery() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = initialize_lease_era(home.path(), &run_id, "invalid-cursor");
    record_begin(home.path(), &run_id, "invalid-cursor").await;
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor is idle");
    guard.replace(&uuid::Uuid::nil().to_string()).unwrap();
    drop(guard);
    fs::write(
        home.path()
            .join("workflow-recovery")
            .join("run-directory.cursor"),
        b"not-a-canonical-run-id",
    )
    .expect("corrupt cursor content without changing permissions");

    let report = reconcile_stale_workflow_runs(home.path(), None).await;
    assert_eq!(report.reconciled_run_ids, vec![run_id]);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("invalid workflow recovery cursor"))
    );
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read recovered run")
            .status,
        WorkflowRunStatus::Failed
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_cursor_symlink_fails_closed_without_scanning_runs() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = initialize_lease_era(home.path(), &run_id, "unsafe-cursor");
    record_begin(home.path(), &run_id, "unsafe-cursor").await;
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open cursor");
    let guard = cursor
        .try_lock()
        .expect("lock cursor")
        .expect("cursor is idle");
    guard.replace(&uuid::Uuid::nil().to_string()).unwrap();
    drop(guard);
    let cursor_path = home
        .path()
        .join("workflow-recovery")
        .join("run-directory.cursor");
    fs::remove_file(&cursor_path).expect("remove cursor file");
    let outside = home.path().join("outside-cursor");
    fs::write(&outside, uuid::Uuid::nil().to_string()).expect("write outside cursor");
    symlink(&outside, &cursor_path).expect("replace cursor with symlink");

    let report = reconcile_stale_workflow_runs(home.path(), None).await;
    assert_eq!(report.scanned, 0);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("filesystem recovery disabled"))
    );
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("unsafe run untouched")
            .status,
        WorkflowRunStatus::Running
    );
}

#[tokio::test]
async fn indexed_recovery_rotates_past_over_cap_junk_without_read_dir_ordering() {
    let home = tempfile::tempdir().expect("tempdir");
    let target_id = uuid::Uuid::from_u128(u128::MAX).to_string();
    let runs_root = codex_workflow_journal::storage::runs_root(home.path());
    fs::create_dir_all(&runs_root).expect("create runs root");
    for index in 0..=super::MAX_RECONCILE_RUNS {
        fs::create_dir(runs_root.join(format!("junk-{index:04}")))
            .expect("create noncanonical directory junk");
    }

    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    let mut indexed_runs = (1..=super::MAX_RECONCILE_RUNS)
        .map(|value| {
            let run_id = uuid::Uuid::from_u128(value as u128).to_string();
            super::run_index_params(home.path(), &sample_meta(&run_id, "missing"))
        })
        .collect::<Vec<_>>();
    indexed_runs.push(super::run_index_params(
        home.path(),
        &sample_meta(&target_id, "target"),
    ));
    state
        .rebuild_workflow_runs(&indexed_runs)
        .await
        .expect("seed an over-cap running index");

    let first = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(first.scanned, super::MAX_INDEXED_RECONCILE_RUNS);
    assert_eq!(first.reconciled, 0);
    assert!(first.truncated);
    let missing_id = uuid::Uuid::from_u128(1).to_string();
    let missing = state
        .get_workflow_run(&missing_id)
        .await
        .expect("read missing-artifact projection")
        .expect("missing-artifact row remains discoverable");
    assert_eq!(missing.status, codex_state::WorkflowRunStatus::Running);
    assert!(
        !missing.status.is_final(),
        "recovery must not invent terminal proof"
    );
    let target = initialize_lease_era(home.path(), &target_id, "target");
    record_begin(home.path(), &target_id, "target").await;
    assert_eq!(
        target.read_meta_bounded().expect("read target meta").status,
        WorkflowRunStatus::Running
    );

    let second = reconcile_stale_workflow_runs(home.path(), Some(state.as_ref())).await;
    assert_eq!(second.scanned, super::MAX_RECONCILE_RUNS);
    assert_eq!(second.reconciled, 1);
    assert_eq!(second.reconciled_run_ids, vec![target_id]);
    assert_eq!(
        target
            .read_meta_bounded()
            .expect("read recovered target meta")
            .status,
        WorkflowRunStatus::Failed
    );

    state.close().await;
}

#[tokio::test]
async fn unindexed_recovery_cursor_rotates_past_junk_and_held_runs() {
    let home = tempfile::tempdir().expect("tempdir");
    let runs_root = codex_workflow_journal::storage::runs_root(home.path());
    fs::create_dir_all(&runs_root).expect("create runs root");
    for index in 0..=super::MAX_RECONCILE_RUNS {
        fs::create_dir(runs_root.join(format!("junk-{index:04}")))
            .expect("create noncanonical directory junk");
    }

    let mut held_leases = Vec::new();
    for value in 1..=8 {
        let run_id = uuid::Uuid::from_u128(value).to_string();
        let paths = WorkflowRunPaths::new(home.path(), &run_id);
        paths.create_dir().expect("create held run directory");
        let WorkflowRunLeaseAcquire::Acquired(lease) =
            WorkflowRunLease::try_acquire(&paths).expect("acquire held run lease")
        else {
            panic!("held test run should acquire its lease");
        };
        paths
            .initialize("export default null;", &sample_meta(&run_id, "held"))
            .expect("initialize held run");
        held_leases.push(lease);
    }

    let target_id = uuid::Uuid::from_u128(u128::MAX).to_string();
    let target = initialize_lease_era(home.path(), &target_id, "target");
    record_begin(home.path(), &target_id, "target").await;
    let limits = super::RecoveryLimits {
        total_runs: 8,
        indexed_runs: 0,
    };

    let first = super::reconcile_stale_workflow_runs_with_limits(home.path(), None, limits).await;
    assert_eq!(first.scanned, 8);
    assert_eq!(first.active, 8);
    assert_eq!(first.reconciled, 0);
    assert!(first.truncated);
    assert_eq!(
        target.read_meta_bounded().expect("read target meta").status,
        WorkflowRunStatus::Running
    );

    let second = super::reconcile_stale_workflow_runs_with_limits(home.path(), None, limits).await;
    assert_eq!(second.scanned, 8);
    assert_eq!(second.active, 7);
    assert_eq!(second.reconciled, 1);
    assert_eq!(second.reconciled_run_ids, vec![target_id]);
    assert_eq!(
        target
            .read_meta_bounded()
            .expect("read recovered target meta")
            .status,
        WorkflowRunStatus::Failed
    );

    drop(held_leases);
}

#[tokio::test]
async fn unindexed_publication_before_cursor_restarts_backfill_without_page_race() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut held_leases = Vec::new();
    for value in 2..=10 {
        let run_id = uuid::Uuid::from_u128(value).to_string();
        let paths = WorkflowRunPaths::new(home.path(), &run_id);
        paths.create_dir().expect("create held run directory");
        let WorkflowRunLeaseAcquire::Acquired(lease) =
            WorkflowRunLease::try_acquire(&paths).expect("acquire held lease")
        else {
            panic!("held fixture lease should be available")
        };
        paths
            .initialize("export default null;", &sample_meta(&run_id, "page-race"))
            .expect("initialize held run");
        held_leases.push(lease);
    }
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    let limits = super::RecoveryLimits {
        total_runs: 4,
        indexed_runs: 0,
    };

    let first =
        super::reconcile_stale_workflow_runs_with_limits(home.path(), Some(state.as_ref()), limits)
            .await;
    assert_eq!(first.scanned, 4);
    assert!(first.truncated);

    // Simulate a publisher whose SQLite state is unavailable. It owns the same
    // lock as recovery and durably invalidates the cursor before making a new
    // lexicographically earlier directory visible.
    let early_id = uuid::Uuid::from_u128(1).to_string();
    let cursor = WorkflowRecoveryCursor::open(home.path()).expect("open publication cursor");
    let guard = cursor
        .try_lock()
        .expect("lock publication cursor")
        .expect("recovery released the cursor between pages");
    guard
        .invalidate()
        .expect("invalidate cursor before publish");
    let early_paths = WorkflowRunPaths::new(home.path(), &early_id);
    early_paths
        .create_dir()
        .expect("create early run directory");
    let WorkflowRunLeaseAcquire::Acquired(early_lease) =
        WorkflowRunLease::try_acquire(&early_paths).expect("acquire early run lease")
    else {
        panic!("early fixture lease should be available")
    };
    early_paths
        .initialize("export default null;", &sample_meta(&early_id, "page-race"))
        .expect("publish early artifacts");
    drop(guard);

    let second =
        super::reconcile_stale_workflow_runs_with_limits(home.path(), Some(state.as_ref()), limits)
            .await;
    assert_eq!(second.scanned, 4);
    assert!(second.active > 0);
    assert_eq!(
        state
            .get_workflow_run(&early_id)
            .await
            .expect("read early projection")
            .expect("cursor reset included lexically earlier run")
            .status,
        codex_state::WorkflowRunStatus::Running
    );

    for _ in 0..3 {
        super::reconcile_stale_workflow_runs_with_limits(home.path(), Some(state.as_ref()), limits)
            .await;
    }
    assert_eq!(
        state
            .prepare_workflow_run_filesystem_index()
            .await
            .expect("read completed page-race index"),
        codex_state::WorkflowRunFilesystemIndexState::Complete
    );

    drop(early_lease);
    drop(held_leases);
    state.close().await;
}

#[tokio::test]
async fn terminal_progress_wins_if_owner_crashes_before_metadata_update() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = initialize_lease_era(home.path(), &run_id, "ordered");
    record_begin(home.path(), &run_id, "ordered").await;
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 25,
            total: Some(1_000),
        }),
    )
    .await
    .expect("record terminal progress");
    let DurableProgressRead::Snapshot(before) =
        read(home.path(), &run_id).await.expect("read before")
    else {
        panic!("expected terminal snapshot");
    };
    assert_eq!(
        paths.read_meta_bounded().expect("read running meta").status,
        WorkflowRunStatus::Running
    );

    let report = reconcile_stale_workflow_run(home.path(), &run_id, None).await;

    assert_eq!(report.reconciled, 1);
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read terminal meta")
            .status,
        WorkflowRunStatus::Completed
    );
    let DurableProgressRead::Snapshot(after) =
        read(home.path(), &run_id).await.expect("read after")
    else {
        panic!("expected terminal snapshot");
    };
    assert_eq!(after, before);

    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "ordered".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Running,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        })
        .await
        .expect("seed stale running projection");

    let projection = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;

    assert_eq!(projection.already_terminal, 1);
    assert_eq!(projection.reconciled, 0);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read repaired state projection")
            .expect("workflow projection exists")
            .status,
        codex_state::WorkflowRunStatus::Completed
    );
    state.close().await;
}

#[tokio::test]
async fn stopped_progress_recovers_meta_and_state_as_stopped() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = initialize_lease_era(home.path(), &run_id, "stopped");
    record_begin(home.path(), &run_id, "stopped").await;
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Shutdown,
            terminal_reason: Some(WorkflowRunTerminalReason::Stopped),
            spent: 25,
            total: Some(1_000),
        }),
    )
    .await
    .expect("record stopped progress");
    let DurableProgressRead::Snapshot(before) = read(home.path(), &run_id)
        .await
        .expect("read stopped progress")
    else {
        panic!("expected stopped snapshot");
    };
    assert_eq!(before.status, DurableRunStatus::Stopped);
    assert_eq!(
        paths.read_meta_bounded().expect("read running meta").status,
        WorkflowRunStatus::Running
    );

    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "stopped".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Running,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        })
        .await
        .expect("seed running projection");

    let report = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;

    assert_eq!(report.reconciled, 1);
    assert_eq!(report.reconciled_run_ids, vec![run_id.clone()]);
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read recovered stopped meta")
            .status,
        WorkflowRunStatus::Stopped
    );
    let DurableProgressRead::Snapshot(after) = read(home.path(), &run_id)
        .await
        .expect("reread stopped progress")
    else {
        panic!("expected stopped snapshot");
    };
    assert_eq!(after, before);
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read stopped state projection")
            .expect("stopped projection exists"),
        codex_state::WorkflowRun {
            run_id: run_id.clone(),
            name: "stopped".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Stopped,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        }
    );
    state.close().await;
}

#[tokio::test]
async fn paused_ledger_repairs_meta_and_state_without_becoming_interrupted() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = initialize_lease_era(home.path(), &run_id, "paused");
    record_begin(home.path(), &run_id, "paused").await;
    record_terminal_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Interrupted,
            terminal_reason: Some(WorkflowRunTerminalReason::Paused),
            spent: 25,
            total: Some(1_000),
        }),
        DurableRunStatus::Paused,
    )
    .await
    .expect("record paused ledger");
    let state =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("initialize state projection");
    state
        .upsert_workflow_run(&codex_state::WorkflowRunUpsertParams {
            run_id: run_id.clone(),
            name: "paused".to_string(),
            script_hash: "blake3:script".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(OWNER_THREAD_ID.to_string()),
            status: codex_state::WorkflowRunStatus::Running,
            created_at: "2026-07-18T00:00:00Z".to_string(),
        })
        .await
        .expect("seed running projection");

    let report = reconcile_stale_workflow_run(home.path(), &run_id, Some(state.as_ref())).await;

    assert_eq!(report.reconciled, 1);
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read repaired paused meta")
            .status,
        WorkflowRunStatus::Paused
    );
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await
            .expect("read paused projection")
            .expect("paused projection exists")
            .status,
        codex_state::WorkflowRunStatus::Paused
    );
    state.close().await;
}
