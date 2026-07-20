use pretty_assertions::assert_eq;

use super::*;

fn run(run_id: &str) -> WorkflowRunUpsertParams {
    WorkflowRunUpsertParams {
        run_id: run_id.to_string(),
        name: "recovery".to_string(),
        script_hash: "blake3:script".to_string(),
        script_path: format!("/tmp/{run_id}/script.js"),
        parent_run_id: None,
        resumed_from_run_id: None,
        owner_thread_id: None,
        status: WorkflowRunStatus::Running,
        created_at: "2026-07-19T00:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn recovery_batches_rotate_surviving_rows_deterministically() -> anyhow::Result<()> {
    let home = crate::runtime::test_support::unique_temp_dir();
    let runtime = StateRuntime::init(home, "test-provider".to_string()).await?;
    runtime
        .rebuild_workflow_runs(&[run("run-c"), run("run-a"), run("run-b")])
        .await?;

    assert_eq!(
        runtime.claim_running_workflow_runs_for_recovery(2).await?,
        WorkflowRunRecoveryBatch {
            run_ids: vec!["run-a".to_string(), "run-b".to_string()],
            has_more: true,
        }
    );
    assert_eq!(
        runtime.claim_running_workflow_runs_for_recovery(2).await?,
        WorkflowRunRecoveryBatch {
            run_ids: vec!["run-c".to_string(), "run-a".to_string()],
            has_more: true,
        }
    );

    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn filesystem_index_backfill_resets_once_then_becomes_complete() -> anyhow::Result<()> {
    let home = crate::runtime::test_support::unique_temp_dir();
    let runtime = StateRuntime::init(home, "test-provider".to_string()).await?;

    assert_eq!(
        runtime.prepare_workflow_run_filesystem_index().await?,
        WorkflowRunFilesystemIndexState::ResetCursor
    );
    assert_eq!(
        runtime.prepare_workflow_run_filesystem_index().await?,
        WorkflowRunFilesystemIndexState::ContinueCursor
    );
    assert_eq!(
        runtime.finish_workflow_run_filesystem_index_cycle().await?,
        WorkflowRunFilesystemCycleResult::Complete
    );
    assert_eq!(
        runtime.prepare_workflow_run_filesystem_index().await?,
        WorkflowRunFilesystemIndexState::Complete
    );

    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn unresolved_filesystem_cycle_restarts_before_completion() -> anyhow::Result<()> {
    let home = crate::runtime::test_support::unique_temp_dir();
    let runtime = StateRuntime::init(home, "test-provider".to_string()).await?;

    runtime.prepare_workflow_run_filesystem_index().await?;
    runtime
        .note_workflow_run_filesystem_index_unresolved()
        .await?;
    assert_eq!(
        runtime.finish_workflow_run_filesystem_index_cycle().await?,
        WorkflowRunFilesystemCycleResult::Restart
    );
    assert_eq!(
        runtime.prepare_workflow_run_filesystem_index().await?,
        WorkflowRunFilesystemIndexState::ContinueCursor
    );
    assert_eq!(
        runtime.finish_workflow_run_filesystem_index_cycle().await?,
        WorkflowRunFilesystemCycleResult::Complete
    );

    runtime.close().await;
    Ok(())
}
