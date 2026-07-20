use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::ExecuteRequest;
use super::PendingRuntimeMode;
use super::RuntimeCommand;
use super::RuntimeControlCommand;
use super::RuntimeEvent;
use super::WorkflowBudgetHandle;
use super::spawn_runtime;
use super::spawn_runtime_with_budget;
use super::spawn_supervised_runtime_thread;
use crate::FunctionCallOutputContentItem;

/// Test [`WorkflowBudgetHandle`] with a fixed `total` and a mutable `spent`
/// counter the test bumps to simulate subagents consuming tokens mid-run.
/// `remaining()` mirrors `RolloutBudget::remaining` (clamped at 0).
struct FixtureBudget {
    total: i64,
    spent: AtomicI64,
}

impl FixtureBudget {
    fn new(total: i64, spent: i64) -> Arc<Self> {
        Arc::new(Self {
            total,
            spent: AtomicI64::new(spent),
        })
    }

    fn set_spent(&self, spent: i64) {
        self.spent.store(spent, Ordering::SeqCst);
    }
}

impl WorkflowBudgetHandle for FixtureBudget {
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

/// Invocation `args` carrying `budget.total`, the source of `budget.total`
/// (§4 `budget`).
fn workflow_budget_args(total: i64) -> serde_json::Value {
    serde_json::json!({ "budget": { "total": total } })
}

fn execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        workflow: false,
        args: None,
        run_id: None,
        replay_entries: Vec::new(),
        workflow_budget: None,
    }
}

/// A workflow-mode request: identical plumbing to [`execute_request`] but with
/// the explicit `workflow` invocation flag set, which is the only thing that
/// authorizes the `phase`/`log` narrator globals.
fn workflow_execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        workflow: true,
        ..execute_request(source)
    }
}

/// A workflow-mode request carrying invocation `args` JSON and a host-minted
/// `run_id`, exercising the read-only `args` / `workflow.runId` globals.
fn workflow_execute_request_with_args(
    source: &str,
    args: serde_json::Value,
    run_id: &str,
) -> ExecuteRequest {
    ExecuteRequest {
        args: Some(args),
        run_id: Some(run_id.to_string()),
        ..workflow_execute_request(source)
    }
}

/// Collect the ordered `text(...)` outputs from a drained event stream.
fn text_outputs(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

/// Assert the terminal `Result` carried no error.
fn assert_result_ok(events: &[RuntimeEvent]) {
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(
        error_text.is_none(),
        "workflow body must run cleanly, got: {error_text:?}"
    );
}

#[tokio::test]
async fn runtime_thread_panic_before_initialization_is_reported_directly() {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    drop(event_rx);
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    spawn_supervised_runtime_thread(
        event_tx,
        Some(std::sync::Arc::new(move |reason| {
            let _ = failure_tx.send(reason);
        })),
        || panic!("runtime thread panic probe"),
    );

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
            .await
            .expect("runtime failure timeout")
            .expect("runtime failure"),
        "code-mode V8 runtime thread panicked"
    );
}

#[tokio::test]
async fn runtime_thread_panic_is_forwarded_without_owner_supervision() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    spawn_supervised_runtime_thread(
        event_tx,
        /*task_failure_handler*/ None,
        || panic!("runtime thread panic probe"),
    );

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("runtime panic event timeout"),
        Some(RuntimeEvent::ThreadPanicked)
    ));
}

#[tokio::test]
async fn terminate_execution_stops_cpu_bound_module() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
        HashMap::new(),
        execute_request("while (true) {}"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let started_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started_event, RuntimeEvent::Started));

    assert!(runtime_terminate_handle.terminate_execution());

    let result_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let RuntimeEvent::Result { error_text, .. } = result_event else {
        panic!("expected runtime result after termination");
    };
    assert!(error_text.is_some());

    assert!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn pending_mode_freezes_runtime_commands_until_resume() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, runtime_control_tx, _runtime_terminate_handle) = spawn_runtime(
        HashMap::new(),
        execute_request(
            r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("after");
await new Promise(() => {});
"#,
        ),
        event_tx,
        PendingRuntimeMode::PauseUntilResumed,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RuntimeEvent::Started
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RuntimeEvent::Pending
    ));

    runtime_tx
        .send(RuntimeCommand::TimeoutFired { id: 1 })
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .is_err()
    );

    runtime_control_tx
        .send(RuntimeControlCommand::Resume)
        .unwrap();

    let content_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) =
        content_event
    else {
        panic!("expected resumed runtime output");
    };
    assert_eq!(text, "after");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        RuntimeEvent::Pending
    ));

    runtime_control_tx
        .send(RuntimeControlCommand::Terminate)
        .unwrap();
}

/// Drain events until the runtime reports its terminal `Result`, returning
/// the ordered events observed (including the final `Result`).
async fn drain_to_result(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed");
        let is_result = matches!(event, RuntimeEvent::Result { .. });
        events.push(event);
        if is_result {
            return events;
        }
    }
}

#[tokio::test]
async fn workflow_phase_and_log_emit_events_in_call_order() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo', phases: ['plan'] };\n",
        "phase('plan');\n",
        "log('hello');\n",
        "phase('build');\n",
    );
    let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    let narrator: Vec<&RuntimeEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::Phase { .. } | RuntimeEvent::WorkflowLog { .. }
            )
        })
        .collect();

    assert_eq!(narrator.len(), 3, "expected phase/log events: {events:?}");
    assert!(matches!(narrator[0], RuntimeEvent::Phase { title } if title == "plan"));
    assert!(matches!(narrator[1], RuntimeEvent::WorkflowLog { message } if message == "hello"));
    assert!(matches!(narrator[2], RuntimeEvent::Phase { title } if title == "build"));

    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(error_text.is_none(), "workflow body must run cleanly");
}

#[tokio::test]
async fn plain_exec_does_not_install_workflow_globals() {
    // A plain code-mode program must NOT see `phase`/`log` — calling them
    // throws a `ReferenceError`, surfaced as a runtime error. Crucially the
    // source here is *meta-shaped* (it opens with a valid `export const meta`
    // manifest), yet because the request is NOT in workflow mode the narrator
    // globals stay uninstalled: workflow-ness is the explicit invocation flag,
    // never the source shape.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo', phases: ['plan'] };\n",
        "phase('x');\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Phase { .. })),
        "plain exec must not emit workflow phase events: {events:?}"
    );
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("phase is not defined"),
        "expected ReferenceError for missing `phase` global, got: {error_text}"
    );
}

#[tokio::test]
async fn workflow_log_reuses_notify_text_validation() {
    // `log()` shares `notify`'s text plumbing: empty input is rejected with a
    // `log`-specific message rather than emitting an event.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "log('   ');\n",
    );
    let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::WorkflowLog { .. })),
        "empty log must not emit an event: {events:?}"
    );
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("log expects non-empty text"),
        "expected the shared narrator validation error, got: {error_text}"
    );
}

#[tokio::test]
async fn workflow_args_global_exposes_invocation_json() {
    // The invocation JSON is injected read-only as the `args` global; a
    // workflow body reads `args.foo` and receives the value passed at
    // invocation (acceptance: "reads args.foo and receives the value").
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(args.foo));\n",
        "text(JSON.stringify(args.nested));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request_with_args(
            source,
            serde_json::json!({ "foo": "bar", "nested": { "n": 1 } }),
            "run-args-1",
        ),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec!["bar".to_string(), "{\"n\":1}".to_string()],
    );
}

#[tokio::test]
async fn workflow_run_id_global_is_host_minted_and_stable() {
    // `workflow.runId` returns exactly the host-minted value and is stable
    // across reads within the run (acceptance: "returns the host-minted uuid
    // v7 and is stable within a run").
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const a = workflow.runId;\n",
        "const b = workflow.runId;\n",
        "text(a);\n",
        "text(String(a === b));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request_with_args(
            source,
            serde_json::Value::Null,
            "0192f000-0000-7000-8000-0000000000ab",
        ),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec![
            "0192f000-0000-7000-8000-0000000000ab".to_string(),
            "true".to_string(),
        ],
    );
}

#[tokio::test]
async fn workflow_args_and_run_id_are_read_only() {
    // Assignment to `args` or `workflow.runId` must throw or be silently
    // ignored. Wrapping each write in try/catch and reading the value back
    // proves the binding is unchanged under either behavior (acceptance:
    // "assignment throws or is silently ignored, tested").
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "try { args = { foo: 'hacked' }; } catch (_e) {}\n",
        "try { workflow.runId = 'hacked'; } catch (_e) {}\n",
        "text(String(args.foo));\n",
        "text(workflow.runId);\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request_with_args(
            source,
            serde_json::json!({ "foo": "original" }),
            "run-readonly-1",
        ),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec!["original".to_string(), "run-readonly-1".to_string()],
        "read-only args/workflow.runId must be unchanged by assignment",
    );
}

#[tokio::test]
async fn workflow_budget_global_reads_total_from_args_without_handle() {
    // With no budget handle threaded, `budget.total` comes from
    // `args.budget.total`, `spent()` reports `0`, and `remaining()` reports
    // `total` (acceptance: a workflow can read total/spent()/remaining();
    // `budget.total` equals `args.budget.total`).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(budget.total));\n",
        "text(String(budget.spent()));\n",
        "text(String(budget.remaining()));\n",
        "text(String(budget.total === args.budget.total));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request_with_args(source, workflow_budget_args(500), "run-budget-1"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec![
            "500".to_string(),
            "0".to_string(),
            "500".to_string(),
            "true".to_string(),
        ],
    );
}

#[tokio::test]
async fn workflow_budget_global_forwards_to_handle_and_holds_invariant() {
    // A threaded handle backs `spent()`/`remaining()`; `total` matches the
    // handle's configured ceiling, and the `spent() + remaining() == total`
    // invariant holds (acceptance: invariant relative to total).
    let budget = FixtureBudget::new(100, 30);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(budget.total));\n",
        "text(String(budget.spent()));\n",
        "text(String(budget.remaining()));\n",
        "text(String(budget.spent() + budget.remaining() === budget.total));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime_with_budget(
        HashMap::new(),
        workflow_execute_request_with_args(source, workflow_budget_args(100), "run-budget-2"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
        Some(budget as Arc<dyn WorkflowBudgetHandle>),
        /*replay_entries*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec![
            "100".to_string(),
            "30".to_string(),
            "70".to_string(),
            "true".to_string(),
        ],
    );
}

#[tokio::test]
async fn workflow_budget_remaining_is_clamped_at_zero_when_overspent() {
    // Overshoot (spent > total) clamps `remaining()` at 0 (acceptance:
    // remaining clamped at 0).
    let budget = FixtureBudget::new(50, 80);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(budget.spent()));\n",
        "text(String(budget.remaining()));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime_with_budget(
        HashMap::new(),
        workflow_execute_request_with_args(source, workflow_budget_args(50), "run-budget-3"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
        Some(budget as Arc<dyn WorkflowBudgetHandle>),
        /*replay_entries*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec!["80".to_string(), "0".to_string()],
    );
}

#[tokio::test]
async fn workflow_budget_spent_is_live_after_agent_completes() {
    // `budget.spent()` is read live at call time: the script reads it before
    // an `agent()` call (0), the test bumps the shared handle to simulate the
    // subagent consuming tokens, then the script re-reads it after the agent
    // completes and observes the updated spend (acceptance: live values that
    // change after subagents complete turns).
    let budget = FixtureBudget::new(100, 0);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(budget.spent()));\n",
        "await agent('do work');\n",
        "text(String(budget.spent()));\n",
        "text(String(budget.remaining()));\n",
    );
    let (runtime_tx, _ctrl, _handle) = spawn_runtime_with_budget(
        HashMap::new(),
        workflow_execute_request_with_args(source, workflow_budget_args(100), "run-budget-4"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
        Some(Arc::clone(&budget) as Arc<dyn WorkflowBudgetHandle>),
        /*replay_entries*/ None,
    )
    .unwrap();

    // Wait for the `agent()` call, buffering the pre-agent events (including
    // the first `text(String(budget.spent()))` output), then simulate the
    // subagent having spent 40 output tokens before resolving it.
    let mut events = Vec::new();
    let call_id = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed");
        match event {
            RuntimeEvent::AgentCall { id, .. } => break id,
            other => events.push(other),
        }
    };
    budget.set_spent(40);
    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id: call_id,
            result: serde_json::json!("ok"),
        })
        .unwrap();

    events.extend(drain_to_result(&mut event_rx).await);
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec!["0".to_string(), "40".to_string(), "60".to_string()],
    );
}

#[tokio::test]
async fn workflow_budget_total_is_read_only() {
    // Assignment to `budget.total` (and to the `budget` binding itself) must
    // throw or be silently ignored; reading back proves the value is
    // unchanged and still equals `args.budget.total` (acceptance:
    // `budget.total` read-only, equals `args.budget.total`).
    let budget = FixtureBudget::new(250, 10);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "try { budget.total = 999; } catch (_e) {}\n",
        "try { budget = { total: 0 }; } catch (_e) {}\n",
        "text(String(budget.total));\n",
        "text(String(budget.total === args.budget.total));\n",
    );
    let (_tx, _ctrl, _handle) = spawn_runtime_with_budget(
        HashMap::new(),
        workflow_execute_request_with_args(source, workflow_budget_args(250), "run-budget-5"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
        Some(budget as Arc<dyn WorkflowBudgetHandle>),
        /*replay_entries*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec!["250".to_string(), "true".to_string()],
        "read-only budget.total must be unchanged by assignment",
    );
}

#[tokio::test]
async fn plain_exec_does_not_install_budget_global() {
    // The `budget` global is workflow-only: a plain code-mode exec that reads
    // `budget` sees a `ReferenceError`, never a leaked global.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(budget.total));\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("budget is not defined"),
        "expected ReferenceError for missing `budget` global, got: {error_text}"
    );
}

#[tokio::test]
async fn plain_exec_does_not_install_args_or_workflow_globals() {
    // The `args` and `workflow` globals are workflow-only: a plain code-mode
    // exec that reads `args` sees a `ReferenceError`, never a leaked global.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text(String(args.foo));\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("args is not defined"),
        "expected ReferenceError for missing `args` global, got: {error_text}"
    );
}

/// Drive a workflow-mode source and return the ordered `text(...)` outputs,
/// asserting the terminal `Result` carried no error. Shared by the
/// `parallel()` prelude tests below.
async fn run_workflow_text_outputs(source: &str) -> Vec<String> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    text_outputs(&events)
}

/// Drive a workflow-mode source carrying invocation `args` (and a fixed
/// `run_id`) and return the ordered `text(...)` outputs, asserting the
/// terminal `Result` carried no error. Used by the determinism-prelude
/// seeded-PRNG tests, which opt in via `args.seed`.
async fn run_workflow_text_outputs_with_args(source: &str, args: serde_json::Value) -> Vec<String> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request_with_args(source, args, "run-determinism"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    text_outputs(&events)
}

#[tokio::test]
async fn workflow_isolate_deletes_weakref_and_finalization_registry() {
    // Determinism harden (§7 / R1): `WeakRef` and `FinalizationRegistry` are
    // default-present in bare V8 but expose GC timing, whose ordering is
    // nondeterministic and would diverge on resume. In a workflow isolate both
    // must be stripped so the script cannot even reference them.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text('WeakRef=' + typeof WeakRef);\n",
        "text('FinalizationRegistry=' + typeof FinalizationRegistry);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "WeakRef=undefined".to_string(),
            "FinalizationRegistry=undefined".to_string(),
        ],
        "workflow isolate must delete GC-order-nondeterministic globals",
    );
}

#[tokio::test]
async fn workflow_isolate_removes_wall_clock_timers() {
    // Determinism harden (§7 / R1): wall-clock timers make the command loop
    // interleave `TimeoutFired` with `ToolResponse` in arrival order, a
    // nondeterminism the `Date`/`Math` shims cannot fix. The workflow isolate
    // installs none of `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`
    // (and therefore never spawns the OS timer thread), so all four are
    // `undefined`; workflows orchestrate via `await agent()`/`parallel()`/
    // `pipeline()` instead.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text('setTimeout=' + typeof setTimeout);\n",
        "text('setInterval=' + typeof setInterval);\n",
        "text('clearTimeout=' + typeof clearTimeout);\n",
        "text('clearInterval=' + typeof clearInterval);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "setTimeout=undefined".to_string(),
            "setInterval=undefined".to_string(),
            "clearTimeout=undefined".to_string(),
            "clearInterval=undefined".to_string(),
        ],
        "workflow isolate must not install any wall-clock timer global",
    );
}

#[tokio::test]
async fn plain_exec_keeps_timers_and_weakref() {
    // Regression guard: the determinism harden is gated STRICTLY to workflow
    // runs. A plain code-mode exec is unaffected — `setTimeout`/`clearTimeout`
    // remain installed (`function`) and `WeakRef`/`FinalizationRegistry` remain
    // the default-present V8 constructors, exactly as before this ticket.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text('setTimeout=' + typeof setTimeout);\n",
        "text('clearTimeout=' + typeof clearTimeout);\n",
        "text('WeakRef=' + typeof WeakRef);\n",
        "text('FinalizationRegistry=' + typeof FinalizationRegistry);\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec![
            "setTimeout=function".to_string(),
            "clearTimeout=function".to_string(),
            "WeakRef=function".to_string(),
            "FinalizationRegistry=function".to_string(),
        ],
        "plain code-mode exec must keep timers and WeakRef/FinalizationRegistry",
    );
}

#[tokio::test]
async fn workflow_determinism_prelude_throws_on_wall_clock_and_random() {
    // Acceptance (§7 / §2 / R1): inside a workflow isolate the frozen
    // determinism prelude makes every wall-clock/entropy source throw —
    // `Date.now()`, argless `new Date()`, `Date()` called as a plain
    // function, and (with no `args.seed`) `Math.random()`.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const threw = (fn) => { try { fn(); return 'ok'; } catch (e) { return 'threw'; } };\n",
        "text('Date.now=' + threw(() => Date.now()));\n",
        "text('new Date()=' + threw(() => new Date()));\n",
        "text('Date()=' + threw(() => Date()));\n",
        "text('Math.random=' + threw(() => Math.random()));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "Date.now=threw".to_string(),
            "new Date()=threw".to_string(),
            "Date()=threw".to_string(),
            "Math.random=threw".to_string(),
        ],
        "workflow determinism prelude must throw on Date.now/argless Date/Math.random",
    );
}

#[tokio::test]
async fn workflow_determinism_prelude_preserves_argful_date_and_parse() {
    // Acceptance (§7): explicit-arg construction and parsing survive so a
    // script can still work with timestamps handed in via `args`. `new
    // Date(x)`, `Date.parse(x)`, and `Date.UTC(...)` all behave normally, and
    // instances remain `instanceof Date`.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text('epoch=' + new Date(0).getTime());\n",
        "text('iso=' + new Date(0).toISOString());\n",
        "text('parse=' + Date.parse('1970-01-01T00:00:00.000Z'));\n",
        "text('utc=' + Date.UTC(1970, 0, 1));\n",
        "text('isDate=' + (new Date(0) instanceof Date));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "epoch=0".to_string(),
            "iso=1970-01-01T00:00:00.000Z".to_string(),
            "parse=0".to_string(),
            "utc=0".to_string(),
            "isDate=true".to_string(),
        ],
        "arg'd Date construction/parse must survive the determinism prelude",
    );
}

#[tokio::test]
async fn workflow_determinism_prelude_shims_are_frozen() {
    // Acceptance (§7): the prelude object graph is frozen — a (strict-mode)
    // module can neither reassign nor delete the shims, and recovering the
    // constructor via `(new Date(0)).constructor` yields the wrapper (whose
    // `now()` throws), never the live wall-clock `Date`.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const threw = (fn) => { try { fn(); return 'ok'; } catch (e) { return 'threw'; } };\n",
        "text('reassignRandom=' + threw(() => { Math.random = () => 0.5; }));\n",
        "text('deleteNow=' + threw(() => { delete Date.now; }));\n",
        "text('reassignDate=' + threw(() => { Date = function () {}; }));\n",
        "text('recoverCtor=' + threw(() => { (new Date(0)).constructor.now(); }));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "reassignRandom=threw".to_string(),
            "deleteNow=threw".to_string(),
            "reassignDate=threw".to_string(),
            "recoverCtor=threw".to_string(),
        ],
        "determinism shims must be non-writable/non-deletable and unrecoverable",
    );
}

#[tokio::test]
async fn workflow_determinism_prelude_seeded_prng_is_deterministic() {
    // Acceptance (§7): with an explicit `args.seed`, `Math.random` is a
    // deterministic splitmix64 PRNG — two runs with the same seed produce the
    // identical sequence (in [0, 1)), and a different seed produces a
    // different sequence.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const seq = [];\n",
        "for (let i = 0; i < 4; i++) seq.push(Math.random());\n",
        "text(JSON.stringify(seq));\n",
        "text('inRange=' + seq.every((x) => x >= 0 && x < 1));\n",
    );
    let seed_42_a =
        run_workflow_text_outputs_with_args(source, serde_json::json!({ "seed": 42 })).await;
    let seed_42_b =
        run_workflow_text_outputs_with_args(source, serde_json::json!({ "seed": 42 })).await;
    let seed_43 =
        run_workflow_text_outputs_with_args(source, serde_json::json!({ "seed": 43 })).await;

    assert_eq!(
        seed_42_a, seed_42_b,
        "same-seed runs must produce an identical PRNG sequence",
    );
    assert_eq!(
        seed_42_a.get(1).map(String::as_str),
        Some("inRange=true"),
        "seeded PRNG values must land in [0, 1)",
    );
    assert_ne!(
        seed_42_a.first(),
        seed_43.first(),
        "a different seed must produce a different PRNG sequence",
    );
}

#[tokio::test]
async fn plain_exec_keeps_live_date_and_math_random() {
    // Regression guard: the determinism prelude is gated STRICTLY to workflow
    // runs. A plain code-mode exec keeps the live `Date`/`Math.random`:
    // `Date.now()` returns a number, `new Date()` is constructible argless,
    // and `Math.random()` returns a value in [0, 1).
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "text('nowIsNumber=' + (typeof Date.now() === 'number'));\n",
        "text('arglessDate=' + (new Date() instanceof Date));\n",
        "const r = Math.random();\n",
        "text('randomInRange=' + (typeof r === 'number' && r >= 0 && r < 1));\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();
    let events = drain_to_result(&mut event_rx).await;
    assert_result_ok(&events);
    assert_eq!(
        text_outputs(&events),
        vec![
            "nowIsNumber=true".to_string(),
            "arglessDate=true".to_string(),
            "randomInRange=true".to_string(),
        ],
        "plain code-mode exec must keep the live Date/Math.random",
    );
}

#[tokio::test]
async fn parallel_returns_results_in_input_order() {
    // Acceptance: `parallel` of N thunks returns an N-length array in input
    // order (position-preserving), independent of completion order.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const results = await parallel([\n",
        "  async () => 'a',\n",
        "  async () => 'b',\n",
        "  async () => 'c',\n",
        "]);\n",
        "text(String(results.length));\n",
        "text(JSON.stringify(results));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["3".to_string(), "[\"a\",\"b\",\"c\"]".to_string()],
    );
}

#[tokio::test]
async fn parallel_throwing_thunk_yields_null_without_failing_siblings() {
    // Acceptance: a thunk that throws yields `null` at its position while its
    // siblings still resolve to their values.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const results = await parallel([\n",
        "  async () => 'ok',\n",
        "  async () => { throw new Error('boom'); },\n",
        "  async () => 'fine',\n",
        "]);\n",
        "text(JSON.stringify(results));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["[\"ok\",null,\"fine\"]".to_string()],
    );
}

#[tokio::test]
async fn parallel_awaits_all_thunks_before_resolving() {
    // Acceptance: `parallel` is a barrier — it awaits ALL thunks before
    // resolving. The thunks complete in a different order than dispatched, yet
    // at resolution every thunk has run (`completed === 3`) and the results
    // stay position-preserving. Out-of-order completion is induced with
    // differing microtask-chain depths (deterministic, FIFO microtask
    // ordering) rather than wall-clock `setTimeout`, which the workflow
    // isolate deliberately does not install (§7 determinism harden): a deeper
    // chain resolves strictly later, so 'a' (depth 30) completes after 'b'
    // (depth 5) and 'c' (depth 15).
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "let completed = 0;\n",
        "const ticks = (n) => {\n",
        "  let p = Promise.resolve();\n",
        "  for (let i = 0; i < n; i++) p = p.then(() => {});\n",
        "  return p;\n",
        "};\n",
        "const mk = (value, depth) => () =>\n",
        "  ticks(depth).then(() => { completed += 1; return value; });\n",
        "const results = await parallel([\n",
        "  mk('a', 30),\n",
        "  mk('b', 5),\n",
        "  mk('c', 15),\n",
        "]);\n",
        "text(String(completed));\n",
        "text(JSON.stringify(results));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["3".to_string(), "[\"a\",\"b\",\"c\"]".to_string()],
        "barrier must await all thunks; results stay position-preserving",
    );
}

#[tokio::test]
async fn parallel_at_cap_boundary_dispatches_all() {
    // The 4096-item boundary is inclusive: exactly 4096 thunks dispatch and
    // resolve without tripping the cap guard.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const thunks = [];\n",
        "for (let i = 0; i < 4096; i++) thunks.push(async () => i);\n",
        "const results = await parallel(thunks);\n",
        "text(String(results.length));\n",
        "text(String(results[0]) + ',' + String(results[4095]));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["4096".to_string(), "0,4095".to_string()],
    );
}

#[tokio::test]
async fn parallel_over_cap_throws_before_any_dispatch() {
    // Acceptance: >4096 items throws a descriptive cap error BEFORE any thunk
    // is dispatched. `dispatched` staying at 0 proves the guard fires ahead of
    // `Array.prototype.map` calling the thunks.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "let dispatched = 0;\n",
        "const thunks = [];\n",
        "for (let i = 0; i < 4097; i++)\n",
        "  thunks.push(() => { dispatched += 1; return Promise.resolve(i); });\n",
        "try {\n",
        "  await parallel(thunks);\n",
        "  text('NO_THROW');\n",
        "} catch (e) {\n",
        "  text(e.constructor.name);\n",
        "  text(String(e.message.includes('4096')));\n",
        "}\n",
        "text('dispatched=' + dispatched);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "RangeError".to_string(),
            "true".to_string(),
            "dispatched=0".to_string(),
        ],
        "cap must throw a descriptive RangeError before dispatching any thunk",
    );
}

#[tokio::test]
async fn parallel_length_lying_proxy_cannot_dispatch_beyond_captured_cap() {
    // A `Proxy` wrapping an array passes `Array.isArray`, so a naive guard that
    // reads `.length` for the cap check and lets `Array.prototype.map` re-read
    // it later could be tricked into dispatching for far more positions than the
    // guard validated. Here the proxy reports `3` on the FIRST length read (the
    // cap guard) and `5000` on every read afterwards, over a backing array of
    // 5000 real thunks. Proxy-safe dispatch snapshots the single captured count
    // (`3`), so exactly 3 thunks are invoked, the result has length 3, and the
    // proxy's `length` trap fires exactly once.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "let dispatched = 0;\n",
        "const real = [];\n",
        "for (let i = 0; i < 5000; i++)\n",
        "  real.push(() => { dispatched += 1; return Promise.resolve(i); });\n",
        "let reads = 0;\n",
        "const proxy = new Proxy(real, {\n",
        "  get(target, prop, recv) {\n",
        "    if (prop === 'length') {\n",
        "      reads += 1;\n",
        "      return reads === 1 ? 3 : 5000;\n",
        "    }\n",
        "    return Reflect.get(target, prop, recv);\n",
        "  },\n",
        "});\n",
        "const results = await parallel(proxy);\n",
        "text('dispatched=' + dispatched);\n",
        "text('len=' + results.length);\n",
        "text('reads=' + reads);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "dispatched=3".to_string(),
            "len=3".to_string(),
            "reads=1".to_string(),
        ],
        "a length-lying proxy must not dispatch beyond the single captured cap",
    );
}

#[tokio::test]
async fn plain_exec_does_not_install_parallel_prelude() {
    // `parallel` is gated on the workflow flag exactly like the other
    // narrator globals: a plain code-mode exec that calls it sees a
    // `ReferenceError`, never a leaked binding.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "await parallel([async () => 1]);\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("parallel is not defined"),
        "expected ReferenceError for missing `parallel` global, got: {error_text}"
    );
}

#[tokio::test]
async fn pipeline_returns_results_in_input_order() {
    // Acceptance: `pipeline(items, ...stages)` resolves to a position-
    // preserving array of length `items.length`; each item is threaded through
    // every stage in order.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const results = await pipeline(\n",
        "  ['a', 'b', 'c'],\n",
        "  async (x) => x + '1',\n",
        "  async (x) => x + '2',\n",
        ");\n",
        "text(String(results.length));\n",
        "text(JSON.stringify(results));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["3".to_string(), "[\"a12\",\"b12\",\"c12\"]".to_string(),],
    );
}

#[tokio::test]
async fn pipeline_stages_have_no_barrier_between_items() {
    // Acceptance: `pipeline` is no-barrier — item A can reach stage 3 while
    // item B is still in stage 1. Item B's stage 1 is delayed, so item A runs
    // all three stages to completion before B ever finishes stage 1. A
    // per-stage *barrier* (the wrong semantics) would force every item through
    // stage 1 before any item entered stage 2, ordering `B:s1` ahead of
    // `A:s2`; the no-barrier chain instead emits every `A:*` before `B:s1`.
    // Item B's stage 1 is delayed by a deep microtask chain (deterministic,
    // FIFO microtask ordering) rather than wall-clock `setTimeout`, which the
    // workflow isolate deliberately does not install (§7 determinism harden):
    // the 50-deep chain lets item A run all three of its zero-depth stages to
    // completion before B:s1 ever fires.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const events = [];\n",
        "const ticks = (n) => {\n",
        "  let p = Promise.resolve();\n",
        "  for (let i = 0; i < n; i++) p = p.then(() => {});\n",
        "  return p;\n",
        "};\n",
        "const stage = (n) => (x) =>\n",
        "  ticks(x === 'B' && n === 1 ? 50 : 0).then(() => {\n",
        "    events.push(x + ':s' + n);\n",
        "    return x;\n",
        "  });\n",
        "const results = await pipeline(['A', 'B'], stage(1), stage(2), stage(3));\n",
        "text(JSON.stringify(results));\n",
        "text(events.join(','));\n",
        "const aStage3 = events.indexOf('A:s3');\n",
        "const bStage1 = events.indexOf('B:s1');\n",
        "text(String(aStage3 >= 0 && bStage1 >= 0 && aStage3 < bStage1));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "[\"A\",\"B\"]".to_string(),
            "A:s1,A:s2,A:s3,B:s1,B:s2,B:s3".to_string(),
            "true".to_string(),
        ],
        "item A must reach stage 3 before item B leaves stage 1 (no barrier)",
    );
}

#[tokio::test]
async fn pipeline_stage_throw_drops_only_that_item_to_null() {
    // Acceptance: a stage that throws resolves ONLY that item's position to
    // `null`; sibling items advance through the remaining stages unaffected and
    // keep their slots (position-preserving).
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const results = await pipeline(\n",
        "  ['a', 'b', 'c'],\n",
        "  async (x) => x,\n",
        "  async (x) => {\n",
        "    if (x === 'b') throw new Error('boom');\n",
        "    return x.toUpperCase();\n",
        "  },\n",
        "  async (x) => x + '!',\n",
        ");\n",
        "text(JSON.stringify(results));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["[\"A!\",null,\"C!\"]".to_string()],
    );
}

#[tokio::test]
async fn pipeline_at_cap_boundary_dispatches_all() {
    // The 4096-item boundary is inclusive: exactly 4096 items thread through
    // the stages without tripping the cap guard.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "const items = [];\n",
        "for (let i = 0; i < 4096; i++) items.push(i);\n",
        "const results = await pipeline(items, async (x) => x + 1);\n",
        "text(String(results.length));\n",
        "text(String(results[0]) + ',' + String(results[4095]));\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec!["4096".to_string(), "1,4096".to_string()],
    );
}

#[tokio::test]
async fn pipeline_over_cap_throws_before_any_dispatch() {
    // Acceptance: >4096 items throws a descriptive cap error BEFORE any stage
    // runs. `dispatched` staying at 0 proves the guard fires ahead of
    // `Array.prototype.map` mapping items onto stage chains.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "let dispatched = 0;\n",
        "const items = [];\n",
        "for (let i = 0; i < 4097; i++) items.push(i);\n",
        "const stage = (x) => { dispatched += 1; return Promise.resolve(x); };\n",
        "try {\n",
        "  await pipeline(items, stage);\n",
        "  text('NO_THROW');\n",
        "} catch (e) {\n",
        "  text(e.constructor.name);\n",
        "  text(String(e.message.includes('4096')));\n",
        "}\n",
        "text('dispatched=' + dispatched);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "RangeError".to_string(),
            "true".to_string(),
            "dispatched=0".to_string(),
        ],
        "cap must throw a descriptive RangeError before dispatching any stage",
    );
}

#[tokio::test]
async fn pipeline_length_lying_proxy_cannot_dispatch_beyond_captured_cap() {
    // The `pipeline()` cap must be equally proxy-safe: the proxy reports `2` on
    // the first length read (cap guard) and `5000` afterwards over a 5000-item
    // backing array. Snapshotting the single captured count means exactly 2 item
    // chains dispatch their (only) stage, the result has length 2, and the
    // proxy's `length` trap fires exactly once.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "let dispatched = 0;\n",
        "const real = [];\n",
        "for (let i = 0; i < 5000; i++) real.push(i);\n",
        "let reads = 0;\n",
        "const proxy = new Proxy(real, {\n",
        "  get(target, prop, recv) {\n",
        "    if (prop === 'length') {\n",
        "      reads += 1;\n",
        "      return reads === 1 ? 2 : 5000;\n",
        "    }\n",
        "    return Reflect.get(target, prop, recv);\n",
        "  },\n",
        "});\n",
        "const stage = (x) => { dispatched += 1; return Promise.resolve(x); };\n",
        "const results = await pipeline(proxy, stage);\n",
        "text('dispatched=' + dispatched);\n",
        "text('len=' + results.length);\n",
        "text('reads=' + reads);\n",
    );
    assert_eq!(
        run_workflow_text_outputs(source).await,
        vec![
            "dispatched=2".to_string(),
            "len=2".to_string(),
            "reads=1".to_string(),
        ],
        "a length-lying proxy must not dispatch beyond the single captured cap",
    );
}

#[tokio::test]
async fn plain_exec_does_not_install_pipeline_prelude() {
    // `pipeline` is gated on the workflow flag exactly like `parallel`: a plain
    // code-mode exec that calls it sees a `ReferenceError`, never a leaked
    // binding.
    let source = concat!(
        "export const meta = { name: 'demo', description: 'demo' };\n",
        "await pipeline([1], async (x) => x);\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("pipeline is not defined"),
        "expected ReferenceError for missing `pipeline` global, got: {error_text}"
    );
}
