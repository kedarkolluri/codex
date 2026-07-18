use super::*;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_utils_absolute_path::AbsolutePathBuf;

#[test]
fn agent_status_uses_bounded_buffered_activity() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::CommandExecution {
                id: "command-1".to_string(),
                command: "cargo test -p codex-tui".to_string(),
                cwd: AbsolutePathBuf::try_from("/workspace")
                    .expect("absolute path")
                    .into(),
                process_id: None,
                source: CommandExecutionSource::Agent,
                status: CommandExecutionStatus::Completed,
                command_actions: Vec::new(),
                aggregated_output: Some("unbounded output\n".repeat(10_000)),
                exit_code: Some(0),
                duration_ms: Some(42),
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "message-1".to_string(),
                text: "Finished checking the focused TUI tests.".to_string(),
                phase: None,
                memory_citation: None,
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 2,
        },
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /agent
    Sub-agents running

      • `/root/reviewer`
        $ cargo test -p codex-tui
        Finished checking the focused TUI tests.
    "###);
    assert!(!rendered.contains("unbounded output"));
}

#[test]
fn agent_status_uses_reasoning_summaries_only() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::Reasoning {
                id: "reasoning-with-summary".to_string(),
                summary: vec!["safe summary".to_string()],
                content: vec!["hidden raw reasoning".to_string()],
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::Reasoning {
                id: "reasoning-without-summary".to_string(),
                summary: Vec::new(),
                content: vec!["raw-only reasoning".to_string()],
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 2,
        },
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /agent
    Sub-agents running

      • `/root/reviewer`
        safe summary
    "###);
    assert!(!rendered.contains("hidden raw reasoning"));
    assert!(!rendered.contains("raw-only reasoning"));
}

/// Seed a per-subagent `ThreadEventStore` from the ordinary Phase-1 agent-status data plane
/// (`Item*` notifications only — the same notifications a real spawned subagent emits as it works).
/// No `workflow/*` progress event is used, mirroring UAT-1-min's "Phase-1 runnable, no `workflow/*`
/// events" constraint.
fn subagent_store(thread_id: &str, reasoning: &str, message: &str) -> ThreadEventStore {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::Reasoning {
                id: format!("{thread_id}-reasoning"),
                summary: vec![reasoning.to_string()],
                content: Vec::new(),
            },
            thread_id: thread_id.to_string(),
            turn_id: format!("{thread_id}-turn"),
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: format!("{thread_id}-message"),
                text: message.to_string(),
                phase: None,
                memory_citation: None,
            },
            thread_id: thread_id.to_string(),
            turn_id: format!("{thread_id}-turn"),
            completed_at_ms: 2,
        },
    ));
    store
}

/// UAT-1-min — subagent leaves are live in the existing "Sub-agents running" snapshot, using only
/// the Phase-1 agent-status data plane (no `workflow/*` events).
///
/// A `parallel()` fan-out's subagents each surface in the existing `AgentStatusHistoryCell` (built
/// from `AgentStatusThreadPreview::from_store`) as a live leaf: a dot + the agent's label + its live
/// activity, produced purely from the `Item*` notifications a spawned subagent emits — never from a
/// `workflow/*` progress event (which is Phase-4 work). This proves agent leaves are observable on
/// the Phase-1 data plane ahead of the Phase-4 phase-tree.
///
/// NOTE ON `token count`: the acceptance names "dot + label + token count", but this snapshot
/// surface (`AgentStatusHistoryCell` / `AgentStatusThreadPreview`) renders dot + label + activity
/// preview and does **not** render a per-agent token count today (there is no token/usage field in
/// `agent_status_feed.rs` or the preview). So the leaf-liveness asserted here is dot + label + live
/// activity; a token-count column would be new rendering work on this cell, tracked as a coverage
/// gap rather than asserted vacuously.
#[test]
fn agent_status_shows_fanout_subagents_as_live_leaves_without_workflow_events() {
    let subagents = [
        ("thread-shard-0", "analyzing shard 0", "shard 0 complete"),
        ("thread-shard-1", "analyzing shard 1", "shard 1 complete"),
        ("thread-shard-2", "analyzing shard 2", "shard 2 complete"),
    ];
    // Deterministic workflow subagent nicknames (bare pool names, per the ordinal-derived nickname
    // scheme) used as the leaf labels.
    let labels = ["researcher", "scribe", "critic"];

    let previews = subagents
        .iter()
        .zip(labels)
        .map(|((thread_id, reasoning, message), label)| {
            let store = subagent_store(thread_id, reasoning, message);
            // Every buffered event feeding a leaf is an ordinary agent-status `Item*` notification;
            // no `workflow/*` progress notification is present on the Phase-1 data plane.
            assert!(
                store.buffer.iter().all(|event| matches!(
                    event,
                    ThreadBufferedEvent::Notification(ServerNotification::ItemCompleted(_))
                )),
                "the leaf must be built from Item* notifications only (no workflow/* events)"
            );
            AgentStatusThreadPreview::from_store(label.to_string(), &store)
        })
        .collect::<Vec<_>>();

    let cell = AgentStatusHistoryCell::new(previews);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /agent
    Sub-agents running

      • `researcher`
        analyzing shard 0
        shard 0 complete

      • `scribe`
        analyzing shard 1
        shard 1 complete

      • `critic`
        analyzing shard 2
        shard 2 complete
    "###);

    // Each spawned subagent renders as a live leaf: dot + label + its live activity.
    for (label, (_, reasoning, message)) in labels.iter().zip(subagents.iter()) {
        assert!(
            rendered.contains(&format!("• `{label}`")),
            "subagent {label} should render as a leaf (dot + label)"
        );
        assert!(
            rendered.contains(reasoning) && rendered.contains(message),
            "subagent {label} leaf should show its live activity"
        );
    }

    // No workflow phase skeleton / `workflow/*` markers leak into the Phase-1 snapshot.
    assert!(
        !rendered.to_lowercase().contains("workflow/"),
        "the Phase-1 agent-status snapshot must not surface any workflow/* markers"
    );
    assert!(
        !rendered.contains("Phase") && !rendered.contains("phases"),
        "the Phase-1 agent-status snapshot must not render a workflow phase tree"
    );
}
