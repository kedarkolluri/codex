use super::*;
use crate::runtime::test_support::unique_temp_dir;
use pretty_assertions::assert_eq;

fn run_params(run_id: &str) -> WorkflowRunUpsertParams {
    WorkflowRunUpsertParams {
        run_id: run_id.to_string(),
        name: "triage".to_string(),
        script_hash: "blake3:script".to_string(),
        script_path: "/runs/triage/script.js".to_string(),
        parent_run_id: None,
        resumed_from_run_id: None,
        owner_thread_id: None,
        status: WorkflowRunStatus::Running,
        created_at: "2026-07-18T00:00:00Z".to_string(),
    }
}

fn agent(
    root: &std::path::Path,
    run_id: &str,
    ordinal: u64,
    thread_id: &str,
) -> WorkflowRunAgentUpsertParams {
    WorkflowRunAgentUpsertParams {
        run_id: run_id.to_string(),
        ordinal,
        thread_id: ThreadId::from_string(thread_id).expect("thread id"),
        rollout_path: root.join(format!("rollout-{thread_id}.jsonl")),
    }
}

async fn runtime() -> anyhow::Result<(std::sync::Arc<StateRuntime>, std::path::PathBuf)> {
    let root = unique_temp_dir();
    let runtime = StateRuntime::init(root.clone(), "test-provider".to_string()).await?;
    Ok((runtime, root))
}

#[tokio::test]
async fn upsert_and_list_are_ordered_by_invocation() -> anyhow::Result<()> {
    let (runtime, root) = runtime().await?;
    runtime.upsert_workflow_run(&run_params("run-1")).await?;
    let first = agent(&root, "run-1", 0, "00000000-0000-0000-0000-000000000001");
    let second = agent(&root, "run-1", 1, "00000000-0000-0000-0000-000000000002");
    runtime.upsert_workflow_run_agent(&second).await?;
    runtime.upsert_workflow_run_agent(&first).await?;

    assert_eq!(
        runtime.list_workflow_run_agents("run-1", 10).await?,
        vec![
            WorkflowRunAgent {
                run_id: first.run_id.clone(),
                ordinal: first.ordinal,
                thread_id: first.thread_id,
                rollout_path: first.rollout_path.clone(),
            },
            WorkflowRunAgent {
                run_id: second.run_id.clone(),
                ordinal: second.ordinal,
                thread_id: second.thread_id,
                rollout_path: second.rollout_path.clone(),
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn replace_is_atomic_and_scoped_to_one_run() -> anyhow::Result<()> {
    let (runtime, root) = runtime().await?;
    runtime.upsert_workflow_run(&run_params("run-1")).await?;
    runtime.upsert_workflow_run(&run_params("run-2")).await?;
    let old = agent(&root, "run-1", 0, "00000000-0000-0000-0000-000000000001");
    let other = agent(&root, "run-2", 0, "00000000-0000-0000-0000-000000000002");
    runtime.upsert_workflow_run_agent(&old).await?;
    runtime.upsert_workflow_run_agent(&other).await?;
    let replacement = agent(&root, "run-1", 3, "00000000-0000-0000-0000-000000000003");

    runtime
        .replace_workflow_run_agents("run-1", std::slice::from_ref(&replacement))
        .await?;

    let run_one = runtime.list_workflow_run_agents("run-1", 10).await?;
    assert_eq!(run_one.len(), 1);
    assert_eq!(run_one[0].ordinal, 3);
    assert_eq!(
        runtime.list_workflow_run_agents("run-2", 10).await?.len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn rejects_relative_paths_mismatched_rebuilds_and_unbounded_limits() -> anyhow::Result<()> {
    let (runtime, root) = runtime().await?;
    runtime.upsert_workflow_run(&run_params("run-1")).await?;
    let mut relative = agent(&root, "run-1", 0, "00000000-0000-0000-0000-000000000001");
    relative.rollout_path = "relative.jsonl".into();
    assert!(runtime.upsert_workflow_run_agent(&relative).await.is_err());

    let mismatch = agent(
        &root,
        "other-run",
        0,
        "00000000-0000-0000-0000-000000000002",
    );
    assert!(
        runtime
            .replace_workflow_run_agents("run-1", &[mismatch])
            .await
            .is_err()
    );
    assert!(runtime.list_workflow_run_agents("run-1", 0).await.is_err());
    assert!(
        runtime
            .list_workflow_run_agents("run-1", MAX_RUN_AGENTS + 1)
            .await
            .is_err()
    );
    Ok(())
}
