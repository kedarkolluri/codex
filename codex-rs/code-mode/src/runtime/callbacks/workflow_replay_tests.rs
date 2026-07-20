//! Isolate-level tests for the `P3-resume-prefix-loop` decision in
//! [`agent_callback`]. They seed the runtime with a prior run's journal
//! `agent_call` entries (as `P3-resume-entry` will in production) and drive the
//! real V8 runtime, emulating the cell actor: an `AgentReplay` (cache hit) is
//! settled from the journaled return WITHOUT a "live" spawn, while an `AgentCall`
//! (divergence) is settled by the test's stand-in host. Asserting on which
//! ordinals produced `AgentReplay` vs `AgentCall` is exactly the acceptance
//! criteria ("no subagent spawns", "the divergence ordinal", the latch, and the
//! source-order `Promise.all` mapping).
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::WorkflowBudgetHandle;
use codex_protocol::protocol::WorkflowEvent;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts as JournalOpts;
use codex_workflow_journal::AgentStatus;
use codex_workflow_journal::KeyInputs;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::PendingRuntimeMode;
use super::super::RuntimeCommand;
use super::super::RuntimeEvent;
use super::super::spawn_runtime;
use super::super::spawn_runtime_with_budget;
use crate::FunctionCallOutputContentItem;

/// How a given ordinal was settled: served from the replay cache (no spawn) or
/// dispatched live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Served {
    Replay,
    Live,
}

/// A [`WorkflowBudgetHandle`] whose `spent` counter the replay driver charges
/// for each cache hit's `tokens_spent`, standing in for the host-side
/// run-local replay charge.
struct ReplayBudget {
    total: i64,
    spent: AtomicI64,
}

impl ReplayBudget {
    fn new(total: i64) -> Arc<Self> {
        Arc::new(Self {
            total,
            spent: AtomicI64::new(0),
        })
    }

    fn record_replayed_spent(&self, tokens: i64) {
        self.spent.fetch_add(tokens, Ordering::SeqCst);
    }
}

impl WorkflowBudgetHandle for ReplayBudget {
    fn total(&self) -> i64 {
        self.total
    }
    fn spent(&self) -> i64 {
        self.spent.load(Ordering::SeqCst)
    }
    fn remaining(&self) -> i64 {
        (self.total - self.spent()).max(0)
    }
}

fn workflow_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        workflow: true,
        args: None,
        run_id: None,
        replay_entries: Vec::new(),
        workflow_budget: None,
    }
}

/// The `(prompt, opts)` cache key exactly as [`agent_callback`] computes it, so
/// a seeded entry keyed with this matches a same-`(prompt, opts)` invocation.
fn cache_key(prompt: &str, opts: &AgentCallOpts) -> String {
    KeyInputs {
        prompt,
        model: opts.model.as_deref(),
        effort: opts.effort.as_deref(),
        agent_type: opts.agent_type.as_deref(),
        isolation: opts.isolation.as_deref(),
        schema: opts.schema.as_ref(),
    }
    .cache_key()
}

/// A journaled `agent_call` entry at `ordinal` for a default-opts `prompt`,
/// with `status`, `ret`, and `tokens_spent`. `key` is computed so it matches a
/// same-prompt invocation; a `completed` entry carries the child linkage §7
/// validation requires.
fn entry(
    ordinal: u64,
    prompt: &str,
    status: Option<AgentStatus>,
    ret: serde_json::Value,
    tokens: Option<u64>,
) -> AgentCallLine {
    let completed = status == Some(AgentStatus::Completed);
    AgentCallLine {
        timestamp: None,
        ordinal,
        attempt: 0,
        key: cache_key(prompt, &AgentCallOpts::default()),
        prompt_hash: "blake3:test".to_string(),
        opts: JournalOpts {
            model: None,
            effort: None,
            agent_type: None,
            isolation: None,
            schema_hash: None,
        },
        phase: None,
        label: None,
        child_thread_id: completed.then(|| "th_prior".to_string()),
        rollout_path: completed.then(|| "/prior/rollout.jsonl".to_string()),
        status,
        control_reason: None,
        ret,
        tokens_spent: tokens,
        progress: None,
        completion_seq: None,
    }
}

/// A `completed` entry (the common seed shape).
fn completed(ordinal: u64, prompt: &str, ret: serde_json::Value, tokens: u64) -> AgentCallLine {
    entry(
        ordinal,
        prompt,
        Some(AgentStatus::Completed),
        ret,
        Some(tokens),
    )
}

async fn recv(event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("runtime event timeout")
        .expect("runtime event channel closed")
}

async fn drain_to_result(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    loop {
        let event = recv(event_rx).await;
        let terminal = matches!(event, RuntimeEvent::Result { .. });
        events.push(event);
        if terminal {
            return events;
        }
    }
}

/// Drive a seeded-replay runtime to its `Result`, settling every agent event
/// the way the cell actor + host would: an `AgentReplay` cache hit resolves
/// from the journaled `return` (and, with a budget, re-adds its `tokens_spent`)
/// WITHOUT a spawn; a live `AgentCall` resolves from `live(prompt)`. Returns
/// the ordered `(ordinal, Served)` log and the collected `text(...)` outputs.
async fn drive(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    live: impl Fn(&str) -> serde_json::Value,
    budget: Option<&Arc<ReplayBudget>>,
) -> (Vec<(u64, Served)>, Vec<String>) {
    let mut served = Vec::new();
    let mut text = Vec::new();
    loop {
        let event = recv(event_rx).await;
        match event {
            RuntimeEvent::AgentReplay { id, entry, .. } => {
                served.push((entry.ordinal, Served::Replay));
                if let Some(budget) = budget {
                    budget.record_replayed_spent(entry.tokens_spent.unwrap_or(0) as i64);
                }
                runtime_tx
                    .send(RuntimeCommand::ToolResponse {
                        id,
                        result: entry.ret,
                    })
                    .unwrap();
            }
            RuntimeEvent::AgentCall {
                id,
                ordinal,
                prompt,
                ..
            } => {
                served.push((ordinal, Served::Live));
                runtime_tx
                    .send(RuntimeCommand::ToolResponse {
                        id,
                        result: live(&prompt),
                    })
                    .unwrap();
            }
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text: t }) => {
                text.push(t);
            }
            RuntimeEvent::Result { error_text, .. } => {
                assert!(error_text.is_none(), "workflow body must run cleanly");
                break;
            }
            _ => {}
        }
    }
    (served, text)
}

fn spawn(
    source: &str,
    replay_entries: Vec<AgentCallLine>,
    budget: Option<Arc<ReplayBudget>>,
) -> (
    std::sync::mpsc::Sender<RuntimeCommand>,
    mpsc::UnboundedReceiver<RuntimeEvent>,
) {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _ctrl, _handle) = spawn_runtime_with_budget(
        HashMap::new(),
        workflow_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
        budget.map(|budget| budget as Arc<dyn WorkflowBudgetHandle>),
        Some(replay_entries),
    )
    .unwrap();
    (runtime_tx, event_rx)
}

fn live_marker(prompt: &str) -> serde_json::Value {
    json!(format!("live:{prompt}"))
}

#[tokio::test]
async fn identical_prefix_replays_every_ordinal_with_no_live_spawns() {
    // Acceptance: an identical script + args replays the whole prefix from
    // cache with NO subagent spawns (zero live `AgentCall`).
    let source = r#"
const a = await agent('a');
const b = await agent('b');
text([a, b].join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        completed(1, "b", json!("B"), 20),
    ];
    let (runtime_tx, mut event_rx) = spawn(source, entries, None);
    let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

    assert_eq!(served, vec![(0, Served::Replay), (1, Served::Replay)]);
    assert!(
        served.iter().all(|(_, kind)| *kind == Served::Replay),
        "no ordinal may dispatch live on a full-prefix replay: {served:?}"
    );
    // Returns come from the journal, not the live stand-in.
    assert_eq!(text, vec!["A,B".to_string()]);
}

#[tokio::test]
async fn replay_uses_current_cosmetic_label_and_phase_without_diverging() {
    let source = r#"
phase('active');
text(String(await agent('a', { label: 'new label', phase: 'override' })));
"#;
    let mut prior = completed(0, "a", json!("A"), 10);
    prior.label = Some("old label".to_string());
    prior.phase = Some("old phase".to_string());
    let (runtime_tx, mut event_rx) = spawn(source, vec![prior], None);

    let (id, node_id, parent_node_id, phase, entry) = loop {
        match recv(&mut event_rx).await {
            RuntimeEvent::AgentReplay {
                id,
                node_id,
                parent_node_id,
                phase,
                entry,
            } => break (id, node_id, parent_node_id, phase, entry),
            RuntimeEvent::AgentCall { .. } => {
                panic!("changing replay cosmetics must not dispatch a live agent")
            }
            RuntimeEvent::Result { .. } => panic!("result preceded the replayed agent"),
            _ => {}
        }
    };

    assert_eq!((node_id, parent_node_id), (0, None));
    assert_eq!(phase.as_deref(), Some("override"));
    assert_eq!(entry.label.as_deref(), Some("new label"));
    assert_eq!(entry.phase.as_deref(), Some("override"));

    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id,
            result: entry.ret.clone(),
        })
        .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    assert!(events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text })
                if text == "A"
        )
    }));
}

#[tokio::test]
async fn edited_call_replays_the_unchanged_prefix_then_goes_live() {
    // Acceptance: the longest unchanged prefix resolves from cache and the
    // first changed call PLUS everything after it runs live. Prior prompts
    // were [a, b, c]; the edited script changes ordinal 1 to `X`. Ordinal 2's
    // prompt `c` still matches the seed but must run live (latch).
    let source = r#"
const r0 = await agent('a');
const r1 = await agent('X');
const r2 = await agent('c');
text([r0, r1, r2].join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        completed(1, "b", json!("B"), 20),
        completed(2, "c", json!("C"), 30),
    ];
    let (runtime_tx, mut event_rx) = spawn(source, entries, None);
    let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

    assert_eq!(
        served,
        vec![(0, Served::Replay), (1, Served::Live), (2, Served::Live)],
        "divergence is at ordinal 1; ordinal 2 runs live despite a matching key",
    );
    assert_eq!(text, vec!["A,live:X,live:c".to_string()]);
}

#[tokio::test]
async fn non_completed_status_forces_divergence_at_that_ordinal() {
    // Acceptance: a cached entry whose `status != completed` forces divergence
    // at that ordinal even though the prompt/key are unchanged.
    let source = r#"
const r0 = await agent('a');
const r1 = await agent('b');
text([r0, r1].join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        // In-flight/interrupted at resume time: status is null, so it re-runs.
        entry(1, "b", None, json!(null), None),
    ];
    let (runtime_tx, mut event_rx) = spawn(source, entries, None);
    let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

    assert_eq!(served, vec![(0, Served::Replay), (1, Served::Live)]);
    assert_eq!(text, vec!["A,live:b".to_string()]);
}

#[tokio::test]
async fn replay_never_re_enables_after_the_first_divergence() {
    // Acceptance: `replay_active` flips false at the first divergence and never
    // re-enables, even when a LATER ordinal's key coincidentally matches. Here
    // ordinal 0 diverges (prompt `Z` != seeded `a`); ordinals 1 and 2 keep the
    // seeded prompts (matching keys) yet all run live.
    let source = r#"
const r0 = await agent('Z');
const r1 = await agent('b');
const r2 = await agent('c');
text([r0, r1, r2].join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        completed(1, "b", json!("B"), 20),
        completed(2, "c", json!("C"), 30),
    ];
    let (runtime_tx, mut event_rx) = spawn(source, entries, None);
    let (served, _text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

    assert_eq!(
        served,
        vec![(0, Served::Live), (1, Served::Live), (2, Served::Live)],
        "a divergence at ordinal 0 forces every later ordinal live despite matching keys",
    );
}

#[tokio::test]
async fn parallel_batch_replays_each_ordinal_in_source_order() {
    // Acceptance: a `Promise.all` batch during replay resolves each cached
    // ordinal in source order (deterministic), matching the original run's
    // results position-for-position — barrier/no-barrier reproduced with no
    // special-casing.
    let source = r#"
const results = await Promise.all([agent('a'), agent('b'), agent('c')]);
text(results.join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        completed(1, "b", json!("B"), 20),
        completed(2, "c", json!("C"), 30),
    ];
    let (runtime_tx, mut event_rx) = spawn(source, entries, None);
    let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

    assert_eq!(
        served,
        vec![
            (0, Served::Replay),
            (1, Served::Replay),
            (2, Served::Replay)
        ],
        "the whole batch is a full-prefix hit, issued in source order",
    );
    assert_eq!(
        text,
        vec!["A,B,C".to_string()],
        "Promise.all preserves the source-order result mapping across replay",
    );
}

#[tokio::test]
async fn replayed_prefix_budget_is_byte_identical() {
    // Acceptance: `budget.spent()` / `budget.remaining()` after the replayed
    // prefix are byte-identical to the original run. The driver charges each
    // cache hit's journaled `tokens_spent` to the run-local fixture, so the
    // script observes the original run's spend curve.
    let source = r#"
const a = await agent('a');
const b = await agent('b');
text(String(budget.spent()));
text(String(budget.remaining()));
text([a, b].join(','));
"#;
    let entries = vec![
        completed(0, "a", json!("A"), 10),
        completed(1, "b", json!("B"), 20),
    ];
    let budget = ReplayBudget::new(100);
    let (runtime_tx, mut event_rx) = spawn(source, entries, Some(Arc::clone(&budget)));
    let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, Some(&budget)).await;

    assert_eq!(served, vec![(0, Served::Replay), (1, Served::Replay)]);
    // 10 + 20 re-added during replay: spent 30 of 100, remaining 70.
    assert_eq!(
        text,
        vec!["30".to_string(), "70".to_string(), "A,B".to_string()],
    );
}

#[tokio::test]
async fn unawaited_parallel_defers_result_until_group_closes() {
    let mut request = workflow_request("parallel([async () => agent('late')]); text('done');");
    request.run_id = Some("run-unawaited".to_string());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        request,
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let (agent_id, node_id, parent_node_id) = loop {
        match recv(&mut event_rx).await {
            RuntimeEvent::AgentCall {
                id,
                node_id,
                parent_node_id,
                ..
            } => break (id, node_id, parent_node_id),
            RuntimeEvent::Result { .. } => panic!("result preceded the unawaited agent"),
            _ => {}
        }
    };
    assert_eq!((node_id, parent_node_id), (1, Some(0)));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                if matches!(recv(&mut event_rx).await, RuntimeEvent::Result { .. }) {
                    break;
                }
            }
        })
        .await
        .is_err(),
        "module completion must remain deferred while unawaited topology is active"
    );

    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id: agent_id,
            result: json!("late-result"),
        })
        .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    let group_end = events.iter().position(|event| {
        matches!(
            event,
            RuntimeEvent::WorkflowProgress(event)
                if matches!(event.as_ref(), WorkflowEvent::GroupEnd(_))
        )
    });
    let result = events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::Result { .. }));
    assert!(group_end.is_some_and(|group_end| result.is_some_and(|result| group_end < result)));
}

#[tokio::test]
async fn async_nested_groups_preserve_parentage_across_awaits_and_overlap() {
    let source = r#"
phase('fanout');
const result = await parallel([
  async () => { await Promise.resolve(); return agent('outer-leaf'); },
  async () => {
await Promise.resolve();
return parallel([async () => { await Promise.resolve(); return agent('nested-leaf'); }]);
  },
]);
text(JSON.stringify(result));
"#;
    let mut request = workflow_request(source);
    request.run_id = Some("run-async-groups".to_string());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        request,
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let mut groups = Vec::new();
    let mut agents = Vec::new();
    while agents.len() < 2 {
        match recv(&mut event_rx).await {
            RuntimeEvent::WorkflowProgress(event) => {
                if let WorkflowEvent::GroupBegin(event) = event.as_ref() {
                    groups.push((event.group_id, event.parent_node_id));
                }
            }
            RuntimeEvent::AgentCall {
                id,
                node_id,
                parent_node_id,
                phase,
                prompt,
                ..
            } => agents.push((id, node_id, parent_node_id, phase, prompt)),
            _ => {}
        }
    }
    assert_eq!(groups, vec![(0, None), (2, Some(0))]);
    assert_eq!(
        agents
            .iter()
            .map(|(_, node_id, parent, phase, prompt)| {
                (*node_id, *parent, phase.as_deref(), prompt.as_str())
            })
            .collect::<Vec<_>>(),
        vec![
            (1, Some(0), Some("fanout"), "outer-leaf"),
            (3, Some(2), Some("fanout"), "nested-leaf"),
        ],
    );

    // Complete the nested leaf first to force inverted completion across overlapping groups.
    for (id, _, _, _, prompt) in agents.into_iter().rev() {
        runtime_tx
            .send(RuntimeCommand::ToolResponse {
                id,
                result: json!(prompt),
            })
            .unwrap();
    }
    let events = drain_to_result(&mut event_rx).await;
    let ended_groups = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::WorkflowProgress(event) => match event.as_ref() {
                WorkflowEvent::GroupEnd(event) => Some(event.group_id),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ended_groups, vec![2, 0]);
}

#[tokio::test]
async fn phase_transition_cannot_close_active_topology() {
    let source = r#"
phase('first');
const active = parallel([() => agent('slow')]);
try {
  phase('second');
} catch (error) {
  text(String(error));
}
await active;
"#;
    let mut request = workflow_request(source);
    request.run_id = Some("run-active-phase".to_string());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        request,
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let agent_id = loop {
        match recv(&mut event_rx).await {
            RuntimeEvent::AgentCall { id, .. } => break id,
            RuntimeEvent::WorkflowProgress(event) => assert!(
                !matches!(event.as_ref(), WorkflowEvent::PhaseEnd(_)),
                "the active phase ended before its topology"
            ),
            RuntimeEvent::Result { .. } => panic!("result preceded the active agent"),
            _ => {}
        }
    };
    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id: agent_id,
            result: json!("done"),
        })
        .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert!(events.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text })
                if text.contains("phase cannot change while workflow groups or agents are active")
        )
    }));
    let group_end = events.iter().position(|event| {
        matches!(
            event,
            RuntimeEvent::WorkflowProgress(event)
                if matches!(event.as_ref(), WorkflowEvent::GroupEnd(_))
        )
    });
    let phase_end = events.iter().position(|event| {
        matches!(
            event,
            RuntimeEvent::WorkflowProgress(event)
                if matches!(event.as_ref(), WorkflowEvent::PhaseEnd(_))
        )
    });
    assert!(
        group_end
            .is_some_and(|group_end| { phase_end.is_some_and(|phase_end| group_end < phase_end) })
    );
}

#[tokio::test]
async fn workflow_log_lifecycle_cap_is_deterministic_at_boundary() {
    let source = format!(
        "for (let i = 0; i < {}; i++) log('bounded');",
        super::super::workflow_progress::MAX_LOG_EVENTS + 1
    );
    let mut request = workflow_request(&source);
    request.run_id = Some("run-log-cap".to_string());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        request,
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    let progress_logs = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::WorkflowProgress(event)
                    if matches!(event.as_ref(), WorkflowEvent::Log(_))
            )
        })
        .count();
    assert_eq!(
        progress_logs as u64,
        super::super::workflow_progress::MAX_LOG_EVENTS
    );
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("terminal result") else {
        panic!("expected terminal result");
    };
    assert!(
        error_text
            .as_deref()
            .is_some_and(|error| error.contains("log event cap exceeded"))
    );
}
