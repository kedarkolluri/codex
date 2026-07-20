#![allow(clippy::expect_used)]
//! End-to-end isolate tests for the cell-actor `agent()` spawn dispatch
//! (`P1-cellactor-spawn-dispatch`).
//!
//! These drive a *real* V8 isolate through the whole in-process code-mode bridge —
//! `agent_callback` -> `RuntimeEvent::AgentCall` -> cell actor -> `CellHost::spawn_agent` ->
//! `SessionRuntimeDelegate` -> `CodeModeSessionDelegate::spawn_agent` -> back as
//! `RuntimeCommand::ToolResponse` -> `resolve_tool_response` -> the isolate promise. The
//! `CodeModeSessionDelegate` here is a deterministic fixture standing in for the core spawn helper
//! (which is proven separately in `core/src/agent/control/spawn_await_tests.rs`), so the fixture's
//! return value is exactly what the workflow observes from `await agent(...)`.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::AgentCallOpts;
use codex_code_mode::AgentSpawnFuture;
use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::InProcessCodeModeSession;
use codex_code_mode::NotificationFuture;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WorkflowBudgetSnapshot;
use codex_code_mode::WorkflowBudgetSnapshotFuture;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

/// A fixture `agent()` host. `respond` maps the requested prompt to the three-way
/// [`AgentSpawnOutcome`]: `Completed` (child final answer), `Failed` (a dead agent -> JS null), or
/// `Rejected` (an admission-time cap/budget rejection -> the isolate throws). Every `spawn_agent`
/// call is counted, and an optional barrier forces all in-flight calls to rendezvous before any
/// resolves (proving the dispatch does not serialize concurrent `agent()` calls).
///
/// `Completed` carries a [`JsonValue`] (not a bare `String`) so a fixture can model a
/// structured-output `agent(prompt, {schema})` call resolving to a JS object as well as a plain
/// schemaless string — exactly what the real core host produces from `finalize_agent_output`.
type RespondFn = Box<dyn Fn(&str) -> AgentSpawnOutcome + Send + Sync>;

struct FixtureAgentDelegate {
    respond: RespondFn,
    spawn_calls: AtomicUsize,
    barrier: Option<Arc<Barrier>>,
    /// Extra per-call delay (ms) applied *after* the barrier, keyed on the parsed prompt index, to
    /// force out-of-order resolution while `Promise.all` preserves input order.
    reverse_delay: bool,
    /// A prompt whose `spawn_agent` never resolves on its own — it blocks until the cell cancels its
    /// callbacks at teardown. Models a hung child that must not serialize its siblings.
    stall_on: Option<String>,
}

impl FixtureAgentDelegate {
    fn new(respond: impl Fn(&str) -> AgentSpawnOutcome + Send + Sync + 'static) -> Self {
        Self {
            respond: Box::new(respond),
            spawn_calls: AtomicUsize::new(0),
            barrier: None,
            reverse_delay: false,
            stall_on: None,
        }
    }

    fn with_barrier(mut self, n: usize) -> Self {
        self.barrier = Some(Arc::new(Barrier::new(n)));
        self.reverse_delay = true;
        self
    }

    fn stall_on(mut self, prompt: &str) -> Self {
        self.stall_on = Some(prompt.to_string());
        self
    }

    fn spawn_calls(&self) -> usize {
        self.spawn_calls.load(Ordering::Acquire)
    }
}

impl CodeModeSessionDelegate for FixtureAgentDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Err("unexpected tool call".to_string()) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn spawn_agent<'a>(
        &'a self,
        _cell_id: CellId,
        _node_id: u64,
        _parent_node_id: Option<u64>,
        _phase: Option<String>,
        prompt: String,
        _ordinal: u64,
        _opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        self.spawn_calls.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if self.stall_on.as_deref() == Some(prompt.as_str()) {
                // Hang until the cell cancels callbacks at teardown; a sibling-serializing dispatch
                // would deadlock here instead of letting the siblings resolve first.
                cancellation_token.cancelled().await;
                return AgentSpawnOutcome::Failed;
            }
            if let Some(barrier) = self.barrier.clone() {
                // Rendezvous: every concurrent call must arrive before any proceeds. If the dispatch
                // serialized these, the barrier would never fill and the test would time out.
                barrier.wait().await;
            }
            if self.reverse_delay {
                // Resolve in reverse index order so `Promise.all` sees out-of-order completion.
                if let Ok(index) = prompt.parse::<u64>() {
                    tokio::time::sleep(Duration::from_millis(16u64.saturating_sub(index))).await;
                }
            }
            (self.respond)(&prompt)
        })
    }

    fn workflow_budget_snapshot<'a>(
        &'a self,
        _cell_id: CellId,
    ) -> WorkflowBudgetSnapshotFuture<'a> {
        let spent = (self.spawn_calls() as u64).saturating_mul(40).min(100);
        Box::pin(async move {
            Ok(Some(WorkflowBudgetSnapshot {
                total: Some(100),
                spent,
                remaining: Some(100 - spent),
            }))
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn workflow_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        // Large yield window so the cell runs to its terminal `Result` (the `agent()` promises
        // resolve promptly) rather than surfacing an intermediate yield frontier.
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
        // Workflow mode installs the `agent()` global.
        workflow: true,
        args: None,
        run_id: None,
        replay_entries: Vec::new(),
        workflow_budget: None,
    }
}

async fn run_workflow(delegate: Arc<FixtureAgentDelegate>, source: &str) -> RuntimeResponse {
    let service = InProcessCodeModeSession::with_delegate(delegate);
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        service
            .execute(workflow_request(source))
            .await
            .expect("start workflow cell")
            .initial_response()
            .await
            .expect("workflow cell result")
    })
    .await
    .expect("workflow completed before timeout");
    service.shutdown().await.expect("shutdown service");
    response
}

fn result_texts(response: &RuntimeResponse) -> Vec<String> {
    let RuntimeResponse::Result {
        content_items,
        error_text,
        ..
    } = response
    else {
        panic!("expected terminal Result, got {response:?}");
    };
    assert_eq!(*error_text, None, "workflow errored: {error_text:?}");
    content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `await agent("p")` resolves to the child's final assistant text.
#[tokio::test]
async fn agent_call_resolves_to_child_final_text() {
    let delegate = Arc::new(FixtureAgentDelegate::new(|prompt| {
        AgentSpawnOutcome::Completed(JsonValue::String(format!("child final for {prompt}")))
    }));
    let response = run_workflow(Arc::clone(&delegate), r#"text(String(await agent("p")));"#).await;

    assert_eq!(
        result_texts(&response),
        vec!["child final for p".to_string()]
    );
    // The `agent()` event dispatched to `spawn_agent` exactly once.
    assert_eq!(delegate.spawn_calls(), 1);
}

/// The in-process host starts from the serializable run snapshot and refreshes
/// the isolate-owned mirror before settling an `agent()` promise.
#[tokio::test]
async fn workflow_budget_refreshes_after_agent_in_process() {
    let delegate = Arc::new(FixtureAgentDelegate::new(|_| {
        AgentSpawnOutcome::Completed(JsonValue::String("done".to_string()))
    }));
    let mut request = workflow_request(
        r#"
text(String(budget.spent()));
text(String(budget.remaining()));
await agent("p");
text(String(budget.spent()));
text(String(budget.remaining()));
"#,
    );
    request.workflow_budget = Some(WorkflowBudgetSnapshot {
        total: Some(100),
        spent: 0,
        remaining: Some(100),
    });
    let service = InProcessCodeModeSession::with_delegate(delegate);
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        service
            .execute(request)
            .await
            .expect("start workflow cell")
            .initial_response()
            .await
            .expect("workflow cell result")
    })
    .await
    .expect("workflow completed before timeout");
    service.shutdown().await.expect("shutdown service");

    assert_eq!(
        result_texts(&response),
        vec![
            "0".to_string(),
            "100".to_string(),
            "40".to_string(),
            "60".to_string(),
        ]
    );
}

/// A structured-output `agent(prompt, {schema})` whose host resolves to a JSON *object* surfaces in
/// the isolate as a real JS object (property-accessible), not a string. The host here stands in for
/// the core `finalize_agent_output` that parses + `jsonschema`-rechecks the child's final message;
/// this test proves the bridge marshals that object end-to-end via `json_to_v8`.
#[tokio::test]
async fn schema_agent_resolves_to_js_object() {
    let delegate = Arc::new(FixtureAgentDelegate::new(|prompt| {
        AgentSpawnOutcome::Completed(
            json!({ "answer": format!("structured for {prompt}"), "score": 7 }),
        )
    }));
    // `.answer`/`.score` only read back if the promise resolved to an object; a string would throw
    // or stringify differently. The `typeof` guard makes the object-ness explicit in the output.
    let response = run_workflow(
        Arc::clone(&delegate),
        r#"
const value = await agent("p", { schema: { type: "object" } });
text(typeof value + ":" + value.answer + ":" + value.score);
"#,
    )
    .await;

    assert_eq!(
        result_texts(&response),
        vec!["object:structured for p:7".to_string()]
    );
    assert_eq!(delegate.spawn_calls(), 1);
}

/// A dead/aborted agent (`None`) resolves the promise to JS `null` and never throws.
#[tokio::test]
async fn dead_agent_resolves_to_null_without_throwing() {
    let delegate = Arc::new(FixtureAgentDelegate::new(|_prompt| {
        AgentSpawnOutcome::Failed
    }));
    // `try/catch` proves the rejection path is never taken: a throw would append "threw".
    let response = run_workflow(
        Arc::clone(&delegate),
        r#"
try {
  const value = await agent("p");
  text(String(value));
} catch (err) {
  text("threw:" + String(err));
}
"#,
    )
    .await;

    assert_eq!(result_texts(&response), vec!["null".to_string()]);
    assert_eq!(delegate.spawn_calls(), 1);
}

/// An admission-time cap/budget rejection (`AgentSpawnOutcome::Rejected`) makes the `agent()` promise
/// *reject* (throw) in the isolate with the rejection message, instead of resolving to `null`. This
/// is the over-cap / over-budget path (e.g. the 1001st `agent()` call) that must not be silently
/// swallowed as a dead agent.
#[tokio::test]
async fn rejected_agent_throws_in_isolate() {
    let delegate = Arc::new(FixtureAgentDelegate::new(|_prompt| {
        AgentSpawnOutcome::Rejected("AgentCapReached".to_string())
    }));
    // The `try/catch` proves the rejection path *is* taken: a resolve-to-null would print "null:..."
    // instead of "threw:...".
    let response = run_workflow(
        Arc::clone(&delegate),
        r#"
try {
  const value = await agent("p");
  text("resolved:" + String(value));
} catch (err) {
  text("threw:" + String(err));
}
"#,
    )
    .await;

    let texts = result_texts(&response);
    assert_eq!(texts.len(), 1, "expected one output line, got {texts:?}");
    assert!(
        texts[0].starts_with("threw:") && texts[0].contains("AgentCapReached"),
        "expected the isolate to throw AgentCapReached, got {texts:?}"
    );
    assert_eq!(delegate.spawn_calls(), 1);
}

/// 16 concurrent `agent()` calls in one `Promise.all` each resolve independently and out-of-order,
/// with `Promise.all` preserving input order — and nothing serializes the dispatch (the shared
/// barrier only fills if all 16 are in flight at once).
#[tokio::test]
async fn sixteen_concurrent_agents_resolve_independently_without_serialization() {
    let delegate = Arc::new(
        FixtureAgentDelegate::new(|prompt| {
            AgentSpawnOutcome::Completed(JsonValue::String(format!("r{prompt}")))
        })
        .with_barrier(16),
    );
    let response = run_workflow(
        Arc::clone(&delegate),
        r#"
const results = await Promise.all(
  Array.from({ length: 16 }, (_, i) => agent(String(i))),
);
text(results.join(","));
"#,
    )
    .await;

    let expected = (0..16)
        .map(|i| format!("r{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(result_texts(&response), vec![expected]);
    // Exactly one dispatch per `agent()` call.
    assert_eq!(delegate.spawn_calls(), 16);
}

/// A single stalled child must not block sibling `agent()` calls: 15 resolve while one never does.
/// The consume loop holds no state that would serialize concurrent in-flight agents, so `stalled`
/// stays pending forever while `Promise.race`/`Promise.all` over the others still completes.
#[tokio::test]
async fn stalled_child_does_not_block_siblings() {
    let delegate = Arc::new(
        FixtureAgentDelegate::new(|prompt| {
            AgentSpawnOutcome::Completed(JsonValue::String(format!("ok{prompt}")))
        })
        .stall_on("stall"),
    );

    // Kick off a genuinely hung child (its `spawn_agent` blocks until teardown) but never await it;
    // the 15 siblings must still resolve independently, proving no serialization.
    let response = run_workflow(
        Arc::clone(&delegate),
        r#"
const stalled = agent("stall");
const siblings = await Promise.all(
  Array.from({ length: 15 }, (_, i) => agent(String(i))),
);
text(siblings.join(","));
"#,
    )
    .await;

    let expected = (0..15)
        .map(|i| format!("ok{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(result_texts(&response), vec![expected]);
    // 15 siblings + the (unawaited) stalled call all dispatched.
    assert_eq!(delegate.spawn_calls(), 16);
}
