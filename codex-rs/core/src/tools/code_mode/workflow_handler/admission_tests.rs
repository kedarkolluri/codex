use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::KEY_ALGO_VERSION;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::canonical_value_hash;
use codex_workflow_journal::prompt_hash;
use codex_workflow_journal::storage::WorkflowRunPaths;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::ResumeSuccessorAdmission;
use super::admit_resume_successor;

const SOURCE: &str = "export const meta = { name: 'triage' };\ntext('resume');";

fn successor_meta(run_id: &str, args: &serde_json::Value) -> WorkflowRunMeta {
    WorkflowRunMeta::new(
        run_id.to_string(),
        None,
        prompt_hash(SOURCE),
        canonical_value_hash(args),
        "triage".to_string(),
        Some(100),
        KEY_ALGO_VERSION,
        "2026-07-19T00:00:00Z".to_string(),
    )
    .with_resumed_from_run_id(uuid::Uuid::now_v7().to_string())
}

#[tokio::test]
async fn crash_torn_expected_line_zero_retries_same_successor_without_overwriting_artifacts() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!({ "private": "value" });
    let meta = successor_meta(&run_id, &args);

    let first = admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
        .await
        .expect("initialize absent successor");
    let ResumeSuccessorAdmission::Start { paths, lease } = first else {
        panic!("absent successor is retryable")
    };
    let expected_header = serde_json::to_vec(&meta).expect("serialize expected line zero");
    let torn_header = &expected_header[..expected_header.len() / 2];
    std::fs::write(paths.journal(), torn_header).expect("simulate torn journal line zero");
    drop(lease);

    let retry = admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
        .await
        .expect("retry exact pre-launch artifacts");
    let ResumeSuccessorAdmission::Start { paths, lease } = retry else {
        panic!("exact pre-launch successor retries under the same id")
    };
    assert_eq!(paths.read_meta_bounded().expect("read meta"), meta);
    assert_eq!(
        paths
            .read_invocation_args_bounded(&meta.args_hash)
            .expect("read private invocation"),
        args
    );
    JournalRecorder::new(&paths, &meta)
        .await
        .expect("retry repairs the authenticated torn journal")
        .shutdown()
        .await
        .expect("close repaired journal");
    let mut expected_journal = expected_header;
    expected_journal.push(b'\n');
    assert_eq!(
        std::fs::read(paths.journal()).expect("read repaired journal"),
        expected_journal
    );
    drop(lease);
}

#[tokio::test]
async fn mismatched_overlong_and_malformed_torn_line_zero_fail_without_mutation() {
    for case in 0..3 {
        let home = tempfile::tempdir().expect("tempdir");
        let run_id = uuid::Uuid::now_v7().to_string();
        let args = json!({ "private": "value" });
        let meta = successor_meta(&run_id, &args);
        let paths = WorkflowRunPaths::new(home.path(), &run_id);
        paths
            .ensure_resume_successor_artifacts(SOURCE, &meta, &args)
            .expect("create exact immutable artifacts");
        let expected = serde_json::to_vec(&meta).expect("serialize expected line zero");
        let bytes = match case {
            0 => {
                let mut different = expected.clone();
                different[1] ^= 1;
                different
            }
            1 => {
                let mut overlong = expected;
                overlong.push(b'x');
                overlong
            }
            2 => b"[".to_vec(),
            _ => unreachable!(),
        };
        std::fs::write(paths.journal(), &bytes).expect("write unsafe torn journal");

        assert!(
            admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(paths.journal()).expect("unsafe journal remains untouched"),
            bytes
        );
    }
}

#[tokio::test]
async fn body_bearing_journal_is_never_repaired_or_reexecuted() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!({ "private": "value" });
    let meta = successor_meta(&run_id, &args);
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths
        .ensure_resume_successor_artifacts(SOURCE, &meta, &args)
        .expect("create exact immutable artifacts");
    let mut bytes = serde_json::to_vec(&meta).expect("serialize expected line zero");
    bytes.extend_from_slice(b"\n{body evidence}\n");
    std::fs::write(paths.journal(), &bytes).expect("write body-bearing journal");

    let admission = admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
        .await
        .expect("body-bearing run is already exposed");
    assert!(matches!(admission, ResumeSuccessorAdmission::Existing));
    assert_eq!(
        std::fs::read(paths.journal()).expect("body-bearing journal remains untouched"),
        bytes
    );
}

#[tokio::test]
async fn nonempty_journal_with_different_identity_is_never_adopted() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!({ "private": "value" });
    let meta = successor_meta(&run_id, &args);
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths
        .ensure_resume_successor_artifacts(SOURCE, &meta, &args)
        .expect("create exact immutable artifacts");
    let mut other = meta.clone();
    other.name = "different identity".to_string();
    let journal = format!(
        "{}\n",
        serde_json::to_string(&other).expect("serialize other meta")
    );
    std::fs::write(paths.journal(), &journal).expect("write mismatched nonempty journal");

    assert!(
        admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(paths.journal()).expect("mismatched journal remains untouched"),
        journal
    );
}

#[tokio::test]
async fn crash_after_exact_running_meta_retries_the_same_successor() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!({ "private": "value" });
    let meta = successor_meta(&run_id, &args);
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths
        .initialize(SOURCE, &meta)
        .expect("simulate crash after script and running meta");
    assert!(!paths.invocation().exists());
    assert!(!paths.journal().exists());

    let retry = admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
        .await
        .expect("retry partial successor");
    let ResumeSuccessorAdmission::Start {
        paths: retried_paths,
        lease,
    } = retry
    else {
        panic!("exact running metadata remains pre-launch retryable")
    };
    assert_eq!(retried_paths.read_meta_bounded().expect("read meta"), meta);
    assert_eq!(
        retried_paths
            .read_invocation_args_bounded(&meta.args_hash)
            .expect("fill missing invocation"),
        args
    );
    drop(lease);
}

#[tokio::test]
async fn live_launched_and_terminal_successors_return_the_existing_id_without_reexecution() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!(null);
    let meta = successor_meta(&run_id, &args);
    let first = admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
        .await
        .expect("initialize successor");
    let ResumeSuccessorAdmission::Start { paths, lease } = first else {
        panic!("fresh successor starts")
    };
    JournalRecorder::new(&paths, &meta)
        .await
        .expect("create exact journal header")
        .shutdown()
        .await
        .expect("close journal header");
    paths
        .mark_execution_launched()
        .expect("commit launch boundary");

    assert!(matches!(
        admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
            .await
            .expect("live duplicate joins"),
        ResumeSuccessorAdmission::Existing
    ));
    drop(lease);
    assert!(matches!(
        admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
            .await
            .expect("launched duplicate joins"),
        ResumeSuccessorAdmission::Existing
    ));

    let terminal_run_id = uuid::Uuid::now_v7().to_string();
    let terminal_meta = successor_meta(&terminal_run_id, &args);
    let terminal =
        admit_resume_successor(home.path(), &terminal_run_id, SOURCE, &terminal_meta, &args)
            .await
            .expect("initialize terminal fixture");
    let ResumeSuccessorAdmission::Start {
        paths: terminal_paths,
        lease: terminal_lease,
    } = terminal
    else {
        panic!("terminal fixture starts")
    };
    JournalRecorder::new(&terminal_paths, &terminal_meta)
        .await
        .expect("create terminal journal header")
        .shutdown()
        .await
        .expect("close terminal journal header");
    terminal_paths
        .update_status(WorkflowRunStatus::Failed)
        .expect("terminalize successor");
    drop(terminal_lease);
    assert!(matches!(
        admit_resume_successor(home.path(), &terminal_run_id, SOURCE, &terminal_meta, &args,)
            .await
            .expect("terminal duplicate joins"),
        ResumeSuccessorAdmission::Existing
    ));
}

#[tokio::test]
async fn corrupt_claimed_successor_is_never_overwritten_or_forked() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let args = json!(null);
    let meta = successor_meta(&run_id, &args);
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create claimed directory");
    std::fs::write(paths.script(), "different source").expect("write corrupt script");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("hold corrupt successor lease")
    else {
        panic!("fresh corrupt fixture lease is available")
    };

    assert!(
        admit_resume_successor(home.path(), &run_id, SOURCE, &meta, &args)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(paths.script()).expect("corrupt bytes remain untouched"),
        "different source"
    );
    drop(lease);
}
