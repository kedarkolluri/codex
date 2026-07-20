use super::*;
use crate::config::test_config;
use codex_state::WorkflowRunStatus;
use codex_state::WorkflowRunUpsertParams;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts;
use codex_workflow_journal::AgentStatus;
use codex_workflow_journal::JournalRecorder;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::fs;

fn workflow_run(
    run_id: &str,
    name: &str,
    created_at: &str,
    parent_run_id: Option<&str>,
    status: WorkflowRunStatus,
) -> WorkflowRunUpsertParams {
    WorkflowRunUpsertParams {
        run_id: run_id.to_string(),
        name: name.to_string(),
        script_hash: format!("blake3:{run_id}"),
        script_path: format!("/saved/{name}.js"),
        parent_run_id: parent_run_id.map(str::to_string),
        resumed_from_run_id: None,
        owner_thread_id: None,
        status,
        created_at: created_at.to_string(),
    }
}

async fn enabled_config(codex_home: &std::path::Path) -> Config {
    let mut config = test_config().await;
    config.codex_home = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(codex_home)
        .expect("temporary codex home is absolute");
    config.sqlite_home = codex_home.to_path_buf();
    config.model_provider_id = "configured-router".to_string();
    config
        .features
        .enable(Feature::Workflow)
        .expect("workflow feature exists");
    config
}

fn write_head_format_run(
    codex_home: &std::path::Path,
    run_id: &str,
    name: &str,
) -> anyhow::Result<(WorkflowRunPaths, Vec<u8>, Vec<u8>)> {
    let paths = WorkflowRunPaths::new(codex_home, run_id);
    paths.create_dir()?;
    let legacy_meta = serde_json::json!({
        "type": "run_meta",
        "run_id": run_id,
        "parent_run_id": null,
        "script_hash": "blake3:legacy-script",
        "args_hash": "blake3:legacy-args",
        "name": name,
        "budget_total": 1_000,
        "key_algo_version": 1,
        "created_at": "2026-07-17T00:00:00Z",
    });
    let meta_bytes = serde_json::to_vec_pretty(&legacy_meta)?;
    let mut journal_bytes = serde_json::to_vec(&legacy_meta)?;
    journal_bytes.push(b'\n');
    fs::write(paths.script(), b"export default null;")?;
    fs::write(paths.meta(), &meta_bytes)?;
    fs::write(paths.journal(), &journal_bytes)?;
    Ok((paths, meta_bytes, journal_bytes))
}

#[tokio::test]
async fn list_runs_reads_configured_home_newest_first_and_honors_limit() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let unrelated_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    let mut resumed_run = workflow_run(
        "run-new",
        "triage",
        "2026-07-18T00:00:00Z",
        Some("nested-parent"),
        WorkflowRunStatus::Running,
    );
    resumed_run.resumed_from_run_id = Some("resume-source".to_string());
    for run in [
        workflow_run(
            "run-old",
            "triage",
            "2026-07-16T00:00:00Z",
            /*parent_run_id*/ None,
            WorkflowRunStatus::Completed,
        ),
        resumed_run,
        workflow_run(
            "run-middle",
            "review",
            "2026-07-17T00:00:00Z",
            /*parent_run_id*/ None,
            WorkflowRunStatus::Failed,
        ),
    ] {
        runtime.upsert_workflow_run(&run).await?;
    }
    runtime.close().await;

    let unrelated_runtime = codex_state::StateRuntime::init(
        unrelated_home.path().to_path_buf(),
        "configured-router".to_string(),
    )
    .await?;
    unrelated_runtime
        .upsert_workflow_run(&workflow_run(
            "wrong-home",
            "wrong",
            "2026-07-19T00:00:00Z",
            /*parent_run_id*/ None,
            WorkflowRunStatus::Completed,
        ))
        .await?;
    unrelated_runtime.close().await;

    let listed = list_runs(&config, /*limit*/ 2).await?;
    assert_eq!(
        listed,
        vec![
            RunSummary {
                run_id: "run-new".to_string(),
                name: "triage".to_string(),
                script_hash: "blake3:run-new".to_string(),
                script_path: "/saved/triage.js".to_string(),
                parent_run_id: Some("nested-parent".to_string()),
                resumed_from_run_id: Some("resume-source".to_string()),
                status: "running".to_string(),
                created_at: "2026-07-18T00:00:00Z".to_string(),
            },
            RunSummary {
                run_id: "run-middle".to_string(),
                name: "review".to_string(),
                script_hash: "blake3:run-middle".to_string(),
                script_path: "/saved/review.js".to_string(),
                parent_run_id: None,
                resumed_from_run_id: None,
                status: "failed".to_string(),
                created_at: "2026-07-17T00:00:00Z".to_string(),
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn list_runs_is_empty_for_a_fresh_configured_home() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;

    assert_eq!(
        list_runs(&config, /*limit*/ 50).await?,
        Vec::<RunSummary>::new()
    );
    Ok(())
}

#[tokio::test]
async fn list_runs_classifies_a_markerless_legacy_run_as_unknown() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let (paths, meta_bytes, journal_bytes) =
        write_head_format_run(configured_home.path(), &run_id, "legacy-list")?;

    let listed = list_runs(&config, /*limit*/ 50).await?;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, run_id);
    assert_eq!(listed[0].status, "unknown");
    assert_eq!(fs::read(paths.meta())?, meta_bytes);
    assert_eq!(fs::read(paths.journal())?, journal_bytes);
    assert!(!paths.lease().exists());
    assert!(!paths.progress().exists());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        runtime
            .get_workflow_run(&listed[0].run_id)
            .await?
            .expect("rebuilt legacy row")
            .status,
        WorkflowRunStatus::Unknown
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn list_runs_rebuilds_an_empty_projection_from_terminal_meta() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let completed_paths = WorkflowRunPaths::new(configured_home.path(), "run-completed");
    let completed_meta = WorkflowRunMeta::new(
        "run-completed".to_string(),
        Some("run-parent".to_string()),
        "blake3:completed".to_string(),
        "blake3:args".to_string(),
        "release-check".to_string(),
        Some(5_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    completed_paths.initialize("export default null;", &completed_meta)?;
    completed_paths.update_status(JournalRunStatus::Completed)?;

    let running_paths = WorkflowRunPaths::new(configured_home.path(), "run-running");
    let running_meta = WorkflowRunMeta::new(
        "run-running".to_string(),
        None,
        "blake3:running".to_string(),
        "blake3:args".to_string(),
        "triage".to_string(),
        Some(0),
        1,
        "2026-07-17T00:00:00Z".to_string(),
    );
    running_paths.initialize("export default null;", &running_meta)?;
    let codex_workflow_journal::WorkflowRunLeaseAcquire::Acquired(_running_lease) =
        codex_workflow_journal::WorkflowRunLease::try_acquire(&running_paths)?
    else {
        panic!("running run should hold its lease");
    };

    let listed = list_runs(&config, /*limit*/ 50).await?;

    assert_eq!(
        listed,
        vec![
            RunSummary {
                run_id: "run-completed".to_string(),
                name: "release-check".to_string(),
                script_hash: "blake3:completed".to_string(),
                script_path: completed_paths.script().display().to_string(),
                parent_run_id: Some("run-parent".to_string()),
                resumed_from_run_id: None,
                status: "completed".to_string(),
                created_at: "2026-07-18T00:00:00Z".to_string(),
            },
            RunSummary {
                run_id: "run-running".to_string(),
                name: "triage".to_string(),
                script_hash: "blake3:running".to_string(),
                script_path: running_paths.script().display().to_string(),
                parent_run_id: None,
                resumed_from_run_id: None,
                status: "running".to_string(),
                created_at: "2026-07-17T00:00:00Z".to_string(),
            },
        ]
    );

    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        runtime
            .get_workflow_run("run-completed")
            .await?
            .expect("rebuilt row")
            .status,
        WorkflowRunStatus::Completed
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn list_runs_rebuilds_stopped_status_from_terminal_meta() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let paths = WorkflowRunPaths::new(configured_home.path(), "run-stopped");
    let meta = WorkflowRunMeta::new(
        "run-stopped".to_string(),
        None,
        "blake3:stopped".to_string(),
        "blake3:args".to_string(),
        "release-check".to_string(),
        Some(5_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    paths.update_status(JournalRunStatus::Stopped)?;

    assert_eq!(
        list_runs(&config, /*limit*/ 50).await?,
        vec![RunSummary {
            run_id: "run-stopped".to_string(),
            name: "release-check".to_string(),
            script_hash: "blake3:stopped".to_string(),
            script_path: paths.script().display().to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            status: "stopped".to_string(),
            created_at: "2026-07-18T00:00:00Z".to_string(),
        }]
    );
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        runtime
            .get_workflow_run("run-stopped")
            .await?
            .expect("rebuilt stopped row")
            .status,
        WorkflowRunStatus::Stopped
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn list_runs_requires_workflow_feature() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let mut config = enabled_config(configured_home.path()).await;
    config
        .features
        .disable(Feature::Workflow)
        .expect("workflow feature exists");

    let error = list_runs(&config, /*limit*/ 50).await.unwrap_err();
    assert!(error.to_string().contains("feature must be enabled"));
    Ok(())
}

#[tokio::test]
async fn list_runs_rejects_unbounded_limits_before_opening_state() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;

    for limit in [0, 1_001] {
        let error = list_runs(&config, /*limit*/ limit).await.unwrap_err();
        assert!(error.to_string().contains("limit must be from 1 to 1000"));
    }
    assert!(!configured_home.path().join("state_5.sqlite").exists());
    Ok(())
}

fn completed_agent_call(
    ordinal: u64,
    thread_id: ThreadId,
    rollout_path: &std::path::Path,
) -> AgentCallLine {
    AgentCallLine {
        timestamp: None,
        ordinal,
        attempt: 0,
        key: format!("blake3:key-{ordinal}"),
        prompt_hash: format!("blake3:prompt-{ordinal}"),
        opts: AgentCallOpts {
            model: Some("fixture-model".to_string()),
            effort: Some("medium".to_string()),
            agent_type: Some("worker".to_string()),
            isolation: None,
            schema_hash: None,
        },
        phase: Some("inspect".to_string()),
        label: Some(format!("agent-{ordinal}")),
        child_thread_id: Some(thread_id.to_string()),
        rollout_path: Some(rollout_path.display().to_string()),
        status: Some(AgentStatus::Completed),
        control_reason: None,
        ret: Value::Null,
        tokens_spent: Some(100 + ordinal),
        progress: None,
        completion_seq: Some(ordinal),
    }
}

#[tokio::test]
async fn list_run_agents_rebuilds_projection_from_authoritative_journal() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "transcript-audit".to_string(),
        Some(5_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    let recorder = JournalRecorder::new(&paths, &meta).await?;
    let thread_zero = ThreadId::from_string("00000000-0000-0000-0000-000000000001")?;
    let thread_one = ThreadId::from_string("00000000-0000-0000-0000-000000000002")?;
    let rollout_zero = configured_home.path().join("rollout-zero.jsonl");
    let rollout_one = configured_home.path().join("rollout-one.jsonl");
    recorder
        .record_agent_call(completed_agent_call(1, thread_one, &rollout_one))
        .await?;
    recorder
        .record_agent_call(completed_agent_call(0, thread_zero, &rollout_zero))
        .await?;
    recorder.shutdown().await?;
    paths.update_status(JournalRunStatus::Completed)?;

    let agents = list_run_agents(&config, &run_id, /*limit*/ 10).await?;

    assert_eq!(
        agents,
        vec![
            RunAgentSummary {
                ordinal: 0,
                thread_id: thread_zero,
                rollout_path: rollout_zero,
            },
            RunAgentSummary {
                ordinal: 1,
                thread_id: thread_one,
                rollout_path: rollout_one,
            },
        ]
    );
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        runtime.list_workflow_run_agents(&run_id, 10).await?.len(),
        2
    );
    assert_eq!(
        runtime
            .get_workflow_run(&run_id)
            .await?
            .expect("run row rebuilt from durable metadata")
            .status,
        WorkflowRunStatus::Completed
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn list_run_agents_does_not_reproject_an_unlocked_running_lease_as_live() -> anyhow::Result<()>
{
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "stale-index".to_string(),
        Some(5_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    let WorkflowRunLeaseAcquire::Acquired(lease) = WorkflowRunLease::try_acquire(&paths)? else {
        panic!("fixture should acquire its lease");
    };
    let recorder = JournalRecorder::new(&paths, &meta).await?;
    recorder.shutdown().await?;
    drop(lease);

    assert_eq!(
        list_run_agents(&config, &run_id, /*limit*/ 10).await?,
        Vec::<RunAgentSummary>::new()
    );
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        runtime
            .get_workflow_run(&run_id)
            .await?
            .expect("reconstructed run row")
            .status,
        WorkflowRunStatus::Unknown
    );
    runtime.close().await;
    Ok(())
}

#[tokio::test]
async fn list_run_agents_rejects_unsafe_ids_and_truncating_limits() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;

    let error = list_run_agents(&config, "../outside", /*limit*/ 10)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid workflow run id"));
    assert!(!configured_home.path().join("state_5.sqlite").exists());

    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "bounded".to_string(),
        Some(5_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    let recorder = JournalRecorder::new(&paths, &meta).await?;
    for (ordinal, suffix) in [(0, 1), (1, 2)] {
        let thread_id = ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012}"))?;
        recorder
            .record_agent_call(completed_agent_call(
                ordinal,
                thread_id,
                &configured_home
                    .path()
                    .join(format!("rollout-{ordinal}.jsonl")),
            ))
            .await?;
    }
    recorder.shutdown().await?;

    let error = list_run_agents(&config, &run_id, /*limit*/ 1)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("more than the requested 1-agent")
    );
    Ok(())
}

#[tokio::test]
async fn inspect_run_combines_progress_journal_binding_and_rollout_tail() -> anyhow::Result<()> {
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::WorkflowAgentBeginEvent;
    use codex_protocol::protocol::WorkflowAgentBoundEvent;
    use codex_protocol::protocol::WorkflowEvent;
    use codex_protocol::protocol::WorkflowGroupBeginEvent;
    use codex_protocol::protocol::WorkflowGroupKind;
    use codex_protocol::protocol::WorkflowPhaseBeginEvent;
    use codex_protocol::protocol::WorkflowRunBeginEvent;
    use codex_workflow_journal::AgentBoundLine;

    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000011")?;
    let rollout_path = configured_home.path().join("child-rollout.jsonl");
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "release-check".to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    let codex_workflow_journal::WorkflowRunLeaseAcquire::Acquired(_lease) =
        codex_workflow_journal::WorkflowRunLease::try_acquire(&paths)?
    else {
        panic!("live workflow should acquire its lease");
    };
    let recorder = JournalRecorder::new(&paths, &meta).await?;
    recorder
        .record_agent_bound(AgentBoundLine {
            timestamp: None,
            ordinal: 0,
            attempt: 0,
            child_thread_id: thread_id.to_string(),
            rollout_path: rollout_path.display().to_string(),
        })
        .await?;
    recorder.shutdown().await?;
    let rollout_line = RolloutLine {
        timestamp: "2026-07-18T00:00:01Z".to_string(),
        ordinal: None,
        item: RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
            message: "Child found the failing invariant.".to_string(),
            phase: None,
            memory_citation: None,
        })),
    };
    std::fs::write(
        &rollout_path,
        format!("{}\n", serde_json::to_string(&rollout_line)?),
    )?;

    for event in [
        WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "release-check".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "inspect".to_string(),
        }),
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: run_id.clone(),
            group_id: 10,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
        WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: run_id.clone(),
            node_id: 11,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(10),
            label: "review".to_string(),
            phase: Some("inspect".to_string()),
            model: "gpt-5".to_string(),
            effort: ReasoningEffort::Medium,
        }),
        WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: run_id.clone(),
            node_id: 11,
            attempt: 0,
            child_thread_id: thread_id.to_string(),
        }),
    ] {
        crate::tools::code_mode::workflow_progress::durable::record_event(
            configured_home.path(),
            &event,
        )
        .await?;
    }

    let view = inspect_run(&config, &run_id).await?;

    assert_eq!(view.name, "release-check");
    assert_eq!(view.status, "running");
    assert!(!view.terminal);
    assert_eq!(view.phases.len(), 1);
    assert_eq!(view.nodes.len(), 2);
    let RunWatchNodeKind::Agent {
        child_thread_id,
        rollout_summary,
        ..
    } = &view.nodes[1].kind
    else {
        panic!("expected projected agent");
    };
    assert_eq!(*child_thread_id, Some(thread_id));
    assert_eq!(
        rollout_summary.as_deref(),
        Some("Child found the failing invariant.")
    );
    assert!(view.unprojected_agents.is_empty());
    assert!(view.warnings.is_empty());
    Ok(())
}

#[tokio::test]
async fn inspect_run_reconciles_a_run_whose_lease_was_dropped() -> anyhow::Result<()> {
    use codex_protocol::protocol::WorkflowEvent;
    use codex_protocol::protocol::WorkflowRunBeginEvent;

    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    paths.create_dir()?;
    let codex_workflow_journal::WorkflowRunLeaseAcquire::Acquired(lease) =
        codex_workflow_journal::WorkflowRunLease::try_acquire(&paths)?
    else {
        panic!("crashed run should start with a lease marker");
    };
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "crashed-watch".to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    crate::tools::code_mode::workflow_progress::durable::record_event(
        configured_home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "crashed-watch".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
    )
    .await?;
    drop(lease);

    let view = inspect_run(&config, &run_id).await?;

    assert!(view.terminal);
    assert_eq!(view.status, "failed");
    assert!(
        view.warnings
            .iter()
            .any(|line| line.contains("former process exited"))
    );
    let state = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    assert_eq!(
        state
            .get_workflow_run(&run_id)
            .await?
            .expect("inspect recovery should project the run")
            .status,
        codex_state::WorkflowRunStatus::Failed
    );
    state.close().await;
    Ok(())
}

#[tokio::test]
async fn inspect_run_stops_after_one_markerless_legacy_snapshot() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let (paths, meta_bytes, journal_bytes) =
        write_head_format_run(configured_home.path(), &run_id, "legacy-watch")?;

    let view = inspect_run(&config, &run_id).await?;

    assert!(view.terminal);
    assert_eq!(view.status, "unknown");
    assert!(
        view.warnings
            .iter()
            .any(|warning| warning.contains("final status is unknown"))
    );
    assert_eq!(fs::read(paths.meta())?, meta_bytes);
    assert_eq!(fs::read(paths.journal())?, journal_bytes);
    assert!(!paths.lease().exists());
    assert!(!paths.progress().exists());
    Ok(())
}

#[tokio::test]
async fn inspect_run_reports_explicit_stop_as_stopped() -> anyhow::Result<()> {
    use codex_protocol::protocol::AgentStatus as ProtocolAgentStatus;
    use codex_protocol::protocol::WorkflowEvent;
    use codex_protocol::protocol::WorkflowRunBeginEvent;
    use codex_protocol::protocol::WorkflowRunEndEvent;
    use codex_protocol::protocol::WorkflowRunTerminalReason;

    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "stopped-watch".to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    crate::tools::code_mode::workflow_progress::durable::record_event(
        configured_home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "stopped-watch".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
    )
    .await?;
    crate::tools::code_mode::workflow_progress::durable::record_event(
        configured_home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: ProtocolAgentStatus::Shutdown,
            terminal_reason: Some(WorkflowRunTerminalReason::Stopped),
            spent: 25,
            total: Some(1_000),
        }),
    )
    .await?;
    paths.update_status(JournalRunStatus::Stopped)?;

    let view = inspect_run(&config, &run_id).await?;

    assert!(view.terminal);
    assert_eq!(view.status, "stopped");
    assert_eq!(
        view.budget,
        Some(RunWatchBudget {
            spent: 25,
            total: Some(1_000)
        })
    );
    assert!(view.warnings.is_empty());
    Ok(())
}

#[tokio::test]
async fn inspect_run_prefers_terminal_metadata_over_stale_running_progress() -> anyhow::Result<()> {
    use codex_protocol::protocol::WorkflowEvent;
    use codex_protocol::protocol::WorkflowRunBeginEvent;

    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "terminal-race".to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    crate::tools::code_mode::workflow_progress::durable::record_event(
        configured_home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "terminal-race".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
    )
    .await?;
    paths.update_status(JournalRunStatus::Failed)?;

    let terminal = inspect_run(&config, &run_id).await?;
    assert!(terminal.terminal);
    assert_eq!(terminal.status, "failed");
    Ok(())
}

#[tokio::test]
async fn inspect_run_falls_back_for_missing_and_corrupt_progress() -> anyhow::Result<()> {
    let configured_home = tempfile::tempdir()?;
    let config = enabled_config(configured_home.path()).await;
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(configured_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id.clone(),
        None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "fallback".to_string(),
        Some(1_000),
        1,
        "2026-07-18T00:00:00Z".to_string(),
    );
    paths.initialize("export default null;", &meta)?;
    paths.update_status(JournalRunStatus::Failed)?;

    let missing = inspect_run(&config, &run_id).await?;
    assert!(missing.terminal);
    assert_eq!(missing.status, "failed");
    assert!(missing.warnings[0].contains("snapshot is missing"));

    std::fs::write(paths.progress(), b"{broken")?;
    let corrupt = inspect_run(&config, &run_id).await?;
    assert!(corrupt.terminal);
    assert_eq!(corrupt.status, "failed");
    assert!(corrupt.warnings[0].contains("snapshot is corrupt"));
    Ok(())
}
