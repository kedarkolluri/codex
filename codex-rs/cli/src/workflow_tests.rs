use super::*;
use codex_core::workflow_cli::RunWatchBudget;
use pretty_assertions::assert_eq;

fn summary() -> RunSummary {
    RunSummary {
        run_id: "12345678-1234-1234-1234-123456789abc".to_string(),
        name: "triage".to_string(),
        script_hash: "blake3:0123456789abcdef".to_string(),
        script_path: "/tmp/saved/triage.js".to_string(),
        parent_run_id: Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()),
        resumed_from_run_id: Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string()),
        status: "completed".to_string(),
        created_at: "2026-07-18T00:00:00Z".to_string(),
    }
}

#[test]
fn render_run_list_has_stable_headers_and_all_discovery_fields() {
    let rendered = render_run_list(&[summary()]);

    let mut lines = rendered.lines();
    assert_eq!(
        lines.next(),
        Some(
            "RUN ID                                STATUS     NAME                      CREATED AT                PARENT RUN ID                         RESUMED FROM RUN ID                   SCRIPT HASH               SCRIPT PATH"
        )
    );
    let row = lines.next().expect("one workflow row");
    for value in [
        "12345678-1234-1234-1234-123456789abc",
        "completed",
        "triage",
        "2026-07-18T00:00:00Z",
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        "blake3:0123456789abcdef",
        "/tmp/saved/triage.js",
    ] {
        assert!(row.contains(value), "missing {value:?} in {row:?}");
    }
    assert_eq!(lines.next(), None);
}

#[test]
fn render_run_list_explains_an_empty_index() {
    let rendered = render_run_list(&[]);
    assert!(rendered.starts_with("RUN ID"));
    assert!(rendered.ends_with("(no workflow runs)\n"));
}

#[test]
fn table_cells_are_single_line_and_bounded() {
    assert_eq!(table_cell("hello\nworld", /*width*/ 20), "hello world");
    assert_eq!(table_cell("123456789", /*width*/ 5), "1234…");
    assert_eq!(table_cell("ééé", /*width*/ 3), "ééé");
}

#[test]
fn list_limit_has_explicit_bounds() {
    assert_eq!(parse_list_limit("1"), Ok(1));
    assert_eq!(
        parse_list_limit(&MAX_WORKFLOW_LIST_LIMIT.to_string()),
        Ok(MAX_WORKFLOW_LIST_LIMIT)
    );
    assert!(parse_list_limit("0").is_err());
    assert!(parse_list_limit("1001").is_err());
    assert!(parse_list_limit("many").is_err());
}

fn watch_view() -> RunWatchView {
    let thread_id = codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000001")
        .expect("thread id");
    RunWatchView {
        run_id: "12345678-1234-1234-1234-123456789abc".to_string(),
        name: "release-check".to_string(),
        status: "running".to_string(),
        terminal: false,
        phases: vec![codex_core::workflow_cli::RunWatchPhase {
            index: 0,
            title: "inspect".to_string(),
            status: "active".to_string(),
            implicit: false,
        }],
        nodes: vec![
            RunWatchNode {
                id: 10,
                parent_node_id: None,
                phase_index: 0,
                kind: RunWatchNodeKind::Group {
                    kind: "parallel".to_string(),
                    item_count: 1,
                    status: "active".to_string(),
                },
            },
            RunWatchNode {
                id: 11,
                parent_node_id: Some(10),
                phase_index: 0,
                kind: RunWatchNodeKind::Agent {
                    label: "review tests".to_string(),
                    model: Some("gpt-5".to_string()),
                    effort: Some("medium".to_string()),
                    child_thread_id: Some(thread_id),
                    status: "running".to_string(),
                    total_tokens: 123,
                    tool_call_count: 4,
                    returned_null: false,
                    rollout_summary: Some("Found one remaining failure.".to_string()),
                },
            },
        ],
        budget: Some(RunWatchBudget {
            spent: 123,
            total: Some(1_000),
        }),
        unprojected_agents: Vec::new(),
        warnings: Vec::new(),
    }
}

#[test]
fn render_watch_shows_live_topology_budget_binding_and_rollout_summary() {
    let rendered = render_run_watch(&watch_view());

    assert!(
        rendered
            .contains("workflow release-check (12345678-1234-1234-1234-123456789abc) [running]")
    );
    assert!(rendered.contains("budget: 123/1000 weighted tokens"));
    assert!(rendered.contains("● phase 1: inspect [active]"));
    assert!(rendered.contains("parallel group (1 items) [active]"));
    assert!(rendered.contains("review tests [running] · 123 tokens · 4 tools · gpt-5/medium"));
    assert!(rendered.contains("thread 00000000-0000-0000-0000-000000000001"));
    assert!(rendered.contains("Found one remaining failure."));
}

#[test]
fn render_watch_distinguishes_unmetered_from_zero_limit() {
    let mut view = watch_view();
    view.budget = Some(RunWatchBudget {
        spent: 718,
        total: None,
    });
    assert!(render_run_watch(&view).contains("budget: 718 weighted tokens (unmetered)"));
    let unmetered_json: serde_json::Value =
        serde_json::from_str(&render_run_watch_json(&view)).expect("unmetered JSON");
    assert_eq!(
        unmetered_json["budget"],
        serde_json::json!({"spent": 718, "total": null})
    );

    view.budget = Some(RunWatchBudget {
        spent: 0,
        total: Some(0),
    });
    assert!(render_run_watch(&view).contains("budget: 0/0 weighted tokens"));
    let zero_json: serde_json::Value =
        serde_json::from_str(&render_run_watch_json(&view)).expect("zero-limit JSON");
    assert_eq!(
        zero_json["budget"],
        serde_json::json!({"spent": 0, "total": 0})
    );
}

#[test]
fn render_watch_has_explicit_corrupt_snapshot_fallback() {
    let thread_id = codex_protocol::ThreadId::from_string("00000000-0000-0000-0000-000000000002")
        .expect("thread id");
    let rendered = render_run_watch(&RunWatchView {
        run_id: "12345678-1234-1234-1234-123456789abc".to_string(),
        name: "triage".to_string(),
        status: "failed".to_string(),
        terminal: true,
        phases: Vec::new(),
        nodes: Vec::new(),
        budget: None,
        unprojected_agents: vec![codex_core::workflow_cli::RunWatchUnprojectedAgent {
            ordinal: 2,
            thread_id,
            rollout_summary: Some("Recovered from the child rollout.".to_string()),
        }],
        warnings: vec!["progress snapshot is corrupt\nunsafe detail".to_string()],
    });

    assert!(rendered.contains("warning: progress snapshot is corrupt unsafe detail"));
    assert!(rendered.contains("(no progress topology yet)"));
    assert!(rendered.contains("journal-linked agents (progress fallback):"));
    assert!(rendered.contains("agent 3 · thread 00000000-0000-0000-0000-000000000002"));
    assert!(rendered.contains("Recovered from the child rollout."));
}

#[test]
fn render_watch_json_is_one_bounded_machine_readable_frame() {
    let rendered = render_run_watch_json(&watch_view());
    assert!(!rendered.contains('\n'));
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("watch JSON");
    assert_eq!(value["runId"], "12345678-1234-1234-1234-123456789abc");
    assert_eq!(value["terminal"], false);
    assert_eq!(value["budget"]["spent"], 123);
    assert_eq!(value["phases"][0]["title"], "inspect");
    assert_eq!(value["nodes"][0]["kind"], "group");
    assert_eq!(value["nodes"][1]["kind"], "agent");
    assert_eq!(
        value["nodes"][1]["childThreadId"],
        "00000000-0000-0000-0000-000000000001"
    );
}

#[test]
fn watch_subcommand_requires_a_run_id() {
    let command = WorkflowCommand::try_parse_from([
        "workflow",
        "watch",
        "12345678-1234-1234-1234-123456789abc",
    ])
    .expect("parse watch");
    let WorkflowAction::Watch(args) = command.action else {
        panic!("expected watch action");
    };
    assert_eq!(args.run_id, "12345678-1234-1234-1234-123456789abc");
    assert!(!args.json);
    let json = WorkflowCommand::try_parse_from([
        "workflow",
        "watch",
        "12345678-1234-1234-1234-123456789abc",
        "--json",
    ])
    .expect("parse JSON watch");
    let WorkflowAction::Watch(args) = json.action else {
        panic!("expected watch action");
    };
    assert!(args.json);
    assert!(WorkflowCommand::try_parse_from(["workflow", "watch"]).is_err());
}
