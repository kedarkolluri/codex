use std::fs;

use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::storage::WorkflowRunPaths;
use pretty_assertions::assert_eq;

use super::DurableNode;
use super::DurableNodeState;
use super::DurablePhaseState;
use super::DurableProgressRead;
use super::DurableRunState;
use super::DurableRunStatus;
use super::MAX_PROGRESS_FILE_BYTES;
use super::MAX_PROGRESS_PHASES;
use super::read;
use super::record_event;
use super::record_terminal_event;

fn run_begin(run_id: &str) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: run_id.to_string(),
        resumed_from_run_id: None,
        name: "triage".to_string(),
        phases: vec!["plan".to_string()],
        args_digest: "blake3:args".to_string(),
    })
}

fn usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        total_tokens,
        input_tokens: total_tokens,
        ..TokenUsage::default()
    }
}

#[tokio::test]
async fn snapshot_write_is_atomic_and_hard_bounded() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();

    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");

    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let metadata = fs::metadata(paths.progress()).expect("progress metadata");
    assert!(metadata.len() <= MAX_PROGRESS_FILE_BYTES);
    let entries = fs::read_dir(paths.run_dir())
        .expect("read run dir")
        .map(|entry| entry.expect("dir entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("progress.json")]);

    let oversized = WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: uuid::Uuid::now_v7().to_string(),
        resumed_from_run_id: None,
        name: "too-many-phases".to_string(),
        phases: vec!["phase".to_string(); MAX_PROGRESS_PHASES + 1],
        args_digest: "blake3:args".to_string(),
    });
    let error = record_event(home.path(), &oversized)
        .await
        .expect_err("phase cap");
    assert!(error.to_string().contains("declares more than"));
}

#[tokio::test]
async fn reducer_merges_out_of_order_terminal_events_without_regression() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let child_thread_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    record_event(
        home.path(),
        &WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "plan".to_string(),
        }),
    )
    .await
    .expect("end phase before begin");
    record_event(
        home.path(),
        &WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id: run_id.clone(),
            group_id: 10,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
    )
    .await
    .expect("end group before begin");
    record_event(
        home.path(),
        &WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: run_id.clone(),
            node_id: 11,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Completed(Some("done".to_string())),
            token_usage: usage(20),
            tool_call_count: 3,
            duration_ms: 900,
            returned_null: false,
        }),
    )
    .await
    .expect("end agent before begin");
    record_event(
        home.path(),
        &WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: run_id.clone(),
            node_id: 11,
            attempt: 0,
            child_thread_id: child_thread_id.clone(),
        }),
    )
    .await
    .expect("bind after terminal");
    record_event(
        home.path(),
        &WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: run_id.clone(),
            node_id: 11,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(10),
            label: "review".to_string(),
            phase: Some("plan".to_string()),
            model: "gpt-5".to_string(),
            effort: ReasoningEffort::Medium,
        }),
    )
    .await
    .expect("begin agent after terminal");
    record_event(
        home.path(),
        &WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: run_id.clone(),
            group_id: 10,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
    )
    .await
    .expect("begin group after terminal");
    record_event(
        home.path(),
        &WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "plan".to_string(),
        }),
    )
    .await
    .expect("begin phase after terminal");

    let DurableProgressRead::Snapshot(snapshot) =
        read(home.path(), &run_id).await.expect("read snapshot")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(
        snapshot.phases.get(&0).expect("phase").state,
        DurablePhaseState::Completed
    );
    let DurableNode::Group(group) = snapshot.topology.get(&10).expect("group") else {
        panic!("expected group");
    };
    assert_eq!(group.parent_node_id, None);
    assert_eq!(group.state, DurableNodeState::Completed);
    let DurableNode::Agent(agent) = snapshot.topology.get(&11).expect("agent") else {
        panic!("expected agent");
    };
    assert_eq!(agent.parent_node_id, Some(10));
    assert_eq!(agent.label.as_deref(), Some("review"));
    assert_eq!(
        agent.child_thread_id.as_deref(),
        Some(child_thread_id.as_str())
    );
    assert_eq!(agent.state, DurableNodeState::Completed);
    assert_eq!(agent.token_usage, usage(20));
    assert_eq!(agent.tool_call_count, 3);
}

#[tokio::test]
async fn run_end_is_durable_and_late_events_cannot_reopen_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 25,
            total: Some(100),
        }),
    )
    .await
    .expect("record terminal");
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 25,
            total: Some(100),
        }),
    )
    .await
    .expect("repeat identical terminal event");

    let DurableProgressRead::Snapshot(snapshot) =
        read(home.path(), &run_id).await.expect("read terminal")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(snapshot.state, DurableRunState::Terminal);
    assert_eq!(snapshot.status, DurableRunStatus::Completed(None));
    assert_eq!(
        snapshot.budget.map(|budget| (budget.spent, budget.total)),
        Some((25, Some(100)))
    );

    let error = record_event(
        home.path(),
        &WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "plan".to_string(),
        }),
    )
    .await
    .expect_err("late event must not reopen terminal run");
    assert!(error.to_string().contains("already terminal"));
}

#[tokio::test]
async fn paused_terminal_projection_is_distinct_from_public_interrupted_status() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    record_terminal_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Interrupted,
            terminal_reason: Some(WorkflowRunTerminalReason::Paused),
            spent: 0,
            total: None,
        }),
        DurableRunStatus::Paused,
    )
    .await
    .expect("record paused terminal");

    let DurableProgressRead::Snapshot(snapshot) = read(home.path(), &run_id)
        .await
        .expect("read paused snapshot")
    else {
        panic!("expected paused snapshot");
    };
    assert_eq!(
        (snapshot.state, snapshot.status),
        (DurableRunState::Terminal, DurableRunStatus::Paused)
    );
}

#[test]
fn terminal_reasons_map_exactly_and_legacy_events_fall_back_to_coarse_status() {
    let run_id = uuid::Uuid::now_v7().to_string();
    let cases = vec![
        (
            AgentStatus::Completed(Some("done".to_string())),
            WorkflowRunTerminalReason::Completed,
            DurableRunStatus::Completed(Some("done".to_string())),
        ),
        (
            AgentStatus::Errored("failed".to_string()),
            WorkflowRunTerminalReason::Failed,
            DurableRunStatus::Errored("failed".to_string()),
        ),
        (
            AgentStatus::Interrupted,
            WorkflowRunTerminalReason::Interrupted,
            DurableRunStatus::Interrupted,
        ),
        (
            AgentStatus::Shutdown,
            WorkflowRunTerminalReason::Stopped,
            DurableRunStatus::Stopped,
        ),
        (
            AgentStatus::Interrupted,
            WorkflowRunTerminalReason::Paused,
            DurableRunStatus::Paused,
        ),
    ];
    for (status, terminal_reason, expected) in cases {
        let event = WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status,
            terminal_reason: Some(terminal_reason),
            spent: 0,
            total: None,
        };
        assert_eq!(
            DurableRunStatus::from_run_end(&event).expect("consistent terminal event"),
            expected
        );
    }

    let legacy = WorkflowRunEndEvent {
        run_id: run_id.clone(),
        status: AgentStatus::Interrupted,
        terminal_reason: None,
        spent: 0,
        total: None,
    };
    assert_eq!(
        DurableRunStatus::from_run_end(&legacy).expect("legacy status fallback"),
        DurableRunStatus::Interrupted
    );

    let contradictory = WorkflowRunEndEvent {
        run_id,
        status: AgentStatus::Completed(None),
        terminal_reason: Some(WorkflowRunTerminalReason::Paused),
        spent: 0,
        total: None,
    };
    assert!(DurableRunStatus::from_run_end(&contradictory).is_err());
}

#[tokio::test]
async fn raw_paused_reason_and_durable_projection_are_equal() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    let raw = WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: run_id.clone(),
        status: AgentStatus::Interrupted,
        terminal_reason: Some(WorkflowRunTerminalReason::Paused),
        spent: 7,
        total: Some(11),
    });
    assert_eq!(
        serde_json::to_value(&raw).expect("serialize raw paused event")["terminal_reason"],
        serde_json::json!("paused")
    );
    record_event(home.path(), &raw)
        .await
        .expect("record paused event without a side-channel override");
    let DurableProgressRead::Snapshot(snapshot) = read(home.path(), &run_id)
        .await
        .expect("read paused snapshot")
    else {
        panic!("expected paused snapshot");
    };
    assert_eq!(snapshot.status, DurableRunStatus::Paused);
}

#[tokio::test]
async fn explicit_stop_is_durable_and_first_terminal_wins() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    let stopped = WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: run_id.clone(),
        status: AgentStatus::Shutdown,
        terminal_reason: Some(WorkflowRunTerminalReason::Stopped),
        spent: 25,
        total: Some(100),
    });
    record_event(home.path(), &stopped)
        .await
        .expect("record explicit stop");
    record_event(home.path(), &stopped)
        .await
        .expect("repeat identical explicit stop");

    let DurableProgressRead::Snapshot(before_conflict) =
        read(home.path(), &run_id).await.expect("read stopped run")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(
        (
            before_conflict.state,
            &before_conflict.status,
            before_conflict
                .budget
                .map(|budget| (budget.spent, budget.total)),
        ),
        (
            DurableRunState::Terminal,
            &DurableRunStatus::Stopped,
            Some((25, Some(100))),
        )
    );
    assert!(
        fs::read_to_string(WorkflowRunPaths::new(home.path(), &run_id).progress())
            .expect("read stopped progress JSON")
            .contains("\"status\":\"stopped\"")
    );

    let error = record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Interrupted,
            terminal_reason: Some(WorkflowRunTerminalReason::Interrupted),
            spent: 25,
            total: Some(100),
        }),
    )
    .await
    .expect_err("a later interruption cannot relabel an explicit stop");
    assert!(error.to_string().contains("conflicting terminal events"));
    let DurableProgressRead::Snapshot(after_conflict) = read(home.path(), &run_id)
        .await
        .expect("reread stopped run")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(after_conflict, before_conflict);
}

#[tokio::test]
async fn legacy_shutdown_progress_status_remains_readable() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    record_event(home.path(), &run_begin(&run_id))
        .await
        .expect("record begin");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(paths.progress()).expect("read seeded progress"))
            .expect("parse seeded progress");
    legacy["state"] = serde_json::json!("terminal");
    legacy["status"] = serde_json::json!("shutdown");
    legacy["budget"] = serde_json::json!({"spent": 0, "total": null});
    fs::write(
        paths.progress(),
        serde_json::to_vec(&legacy).expect("serialize legacy progress"),
    )
    .expect("write legacy progress");

    let DurableProgressRead::Snapshot(snapshot) = read(home.path(), &run_id)
        .await
        .expect("read legacy progress")
    else {
        panic!("expected snapshot");
    };
    assert_eq!(snapshot.status, DurableRunStatus::Shutdown);
}

#[tokio::test]
async fn durable_budget_distinguishes_unmetered_from_zero_limit() {
    for total in [None, Some(0)] {
        let home = tempfile::tempdir().expect("tempdir");
        let run_id = uuid::Uuid::now_v7().to_string();
        record_event(home.path(), &run_begin(&run_id))
            .await
            .expect("record begin");
        record_event(
            home.path(),
            &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                run_id: run_id.clone(),
                status: AgentStatus::Completed(None),
                terminal_reason: Some(WorkflowRunTerminalReason::Completed),
                spent: 0,
                total,
            }),
        )
        .await
        .expect("record terminal");
        let DurableProgressRead::Snapshot(snapshot) =
            read(home.path(), &run_id).await.expect("read terminal")
        else {
            panic!("expected snapshot");
        };
        assert_eq!(
            snapshot.budget.map(|budget| (budget.spent, budget.total)),
            Some((0, total))
        );
    }
}

#[tokio::test]
async fn missing_and_corrupt_snapshots_are_explicit_fallbacks() {
    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    assert_eq!(
        read(home.path(), &run_id).await.expect("missing read"),
        DurableProgressRead::Missing
    );

    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run dir");
    fs::write(paths.progress(), b"{not-json").expect("write corrupt progress");
    let DurableProgressRead::Corrupt(error) =
        read(home.path(), &run_id).await.expect("corrupt read")
    else {
        panic!("expected corrupt result");
    };
    assert!(error.contains("invalid JSON"));
}

#[cfg(unix)]
#[tokio::test]
async fn progress_symlinks_are_rejected_for_reads_and_writes() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("tempdir");
    let run_id = uuid::Uuid::now_v7().to_string();
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    paths.create_dir().expect("create run dir");
    let target = home.path().join("outside.json");
    fs::write(&target, b"{}").expect("write target");
    symlink(&target, paths.progress()).expect("create symlink");

    read(home.path(), &run_id)
        .await
        .expect_err("progress symlink must fail closed");
    assert!(
        record_event(home.path(), &run_begin(&run_id))
            .await
            .is_err()
    );
    assert_eq!(fs::read(target).expect("target intact"), b"{}");
}
