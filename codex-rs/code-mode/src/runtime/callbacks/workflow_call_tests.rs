//! Isolate-level tests for the workflow `agent()` global (P1-agent-callback).
//! These drive the real V8 runtime through [`spawn_runtime`] so the
//! synchronous ordinal stamping and id-keyed resolution are exercised
//! end-to-end, exactly as the acceptance criteria name.
use std::collections::HashMap;
use std::time::Duration;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::ExecuteRequest;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::PendingRuntimeMode;
use super::super::RuntimeCommand;
use super::super::RuntimeEvent;
use super::super::spawn_runtime;
use crate::FunctionCallOutputContentItem;

/// A workflow-mode request — the explicit `workflow` flag is the only thing
/// that authorizes the `agent` global.
fn workflow_execute_request(source: &str) -> ExecuteRequest {
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

/// A plain (non-workflow) request: identical plumbing but without the flag,
/// so the `agent` global is never installed.
fn plain_execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        workflow: false,
        ..workflow_execute_request(source)
    }
}

async fn next_event(event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("runtime event timeout")
        .expect("runtime event channel closed")
}

/// Collect the first `count` [`RuntimeEvent::AgentCall`] events, skipping the
/// `Started`/`Pending` lifecycle events, and return their `(id, ordinal,
/// prompt, opts)` tuples in emission order.
async fn collect_agent_calls(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    count: usize,
) -> Vec<(String, u64, String, AgentCallOpts)> {
    let mut calls = Vec::new();
    while calls.len() < count {
        match next_event(event_rx).await {
            RuntimeEvent::AgentCall {
                id,
                ordinal,
                prompt,
                opts,
                ..
            } => calls.push((id, ordinal, prompt, opts)),
            RuntimeEvent::Started | RuntimeEvent::Pending => {}
            other => panic!("unexpected event before agent calls: {other:?}"),
        }
    }
    calls
}

/// Drain events until the runtime reports its terminal `Result`, returning
/// the ordered events observed (including the final `Result`).
async fn drain_to_result(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    loop {
        let event = next_event(event_rx).await;
        let is_result = matches!(event, RuntimeEvent::Result { .. });
        events.push(event);
        if is_result {
            return events;
        }
    }
}

#[tokio::test]
async fn agent_call_emits_one_pending_promise_event_resolvable_by_id() {
    // `agent("p")` returns a pending Promise and emits exactly one AgentCall.
    // Responding by the emitted id resolves that promise (proving the
    // resolver is retrievable by id from `pending_tool_calls`).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request("text(await agent('solve'));"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let calls = collect_agent_calls(&mut event_rx, 1).await;
    assert_eq!(calls.len(), 1);
    let (id, ordinal, prompt, opts) = &calls[0];
    assert_eq!(id, "agent-0");
    assert_eq!(*ordinal, 0);
    assert_eq!(prompt, "solve");
    assert_eq!(*opts, AgentCallOpts::default());

    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id: id.clone(),
            result: json!("done"),
        })
        .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    // Exactly one AgentCall over the whole run.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::AgentCall { .. }))
            .count(),
        0,
        "the single AgentCall was already consumed before draining: {events:?}"
    );
    let text = events.iter().find_map(|event| match event {
        RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
            Some(text.clone())
        }
        _ => None,
    });
    assert_eq!(text.as_deref(), Some("done"));
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(error_text.is_none(), "workflow body must run cleanly");
}

#[tokio::test]
async fn parallel_agent_calls_stamp_source_order_ordinals_regardless_of_arrival() {
    // Promise.all fires the three agent() thunks synchronously in array
    // order, so ordinals are 0,1,2 in SOURCE order. Responding in REVERSE
    // arrival order must not perturb the ordinals, and Promise.all preserves
    // the source-order mapping of results.
    let source = r#"
const results = await Promise.all([
  agent('a', { label: 'la' }),
  agent('b', { phase: 'ph', agentType: 'reviewer' }),
  agent('c'),
]);
text(results.join(','));
"#;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let calls = collect_agent_calls(&mut event_rx, 3).await;
    let ids: Vec<&str> = calls.iter().map(|(id, ..)| id.as_str()).collect();
    let ordinals: Vec<u64> = calls.iter().map(|(_, ordinal, ..)| *ordinal).collect();
    let prompts: Vec<&str> = calls
        .iter()
        .map(|(_, _, prompt, _)| prompt.as_str())
        .collect();

    assert_eq!(ordinals, vec![0, 1, 2], "ordinals must be source-ordered");
    assert_eq!(ids, vec!["agent-0", "agent-1", "agent-2"]);
    assert_eq!(prompts, vec!["a", "b", "c"]);

    // opts fields are carried through unchanged onto the emitted event.
    assert_eq!(calls[0].3.label.as_deref(), Some("la"));
    assert_eq!(calls[1].3.phase.as_deref(), Some("ph"));
    assert_eq!(calls[1].3.agent_type.as_deref(), Some("reviewer"));
    assert_eq!(calls[2].3, AgentCallOpts::default());

    // Respond in reverse arrival order: agent-2, then agent-1, then agent-0.
    for (id, result) in [
        ("agent-2", json!("C")),
        ("agent-1", json!("B")),
        ("agent-0", json!("A")),
    ] {
        runtime_tx
            .send(RuntimeCommand::ToolResponse {
                id: id.to_string(),
                result,
            })
            .unwrap();
    }

    let events = drain_to_result(&mut event_rx).await;
    let text = events.iter().find_map(|event| match event {
        RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
            Some(text.clone())
        }
        _ => None,
    });
    // Promise.all preserves source-order mapping despite reverse resolution.
    assert_eq!(text.as_deref(), Some("A,B,C"));
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(error_text.is_none(), "workflow body must run cleanly");
}

#[tokio::test]
async fn plain_exec_has_no_agent_global() {
    // Workflow-ness is the explicit flag, never the source shape: a plain
    // exec calling `agent()` sees a ReferenceError.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        plain_execute_request("await agent('x');"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::AgentCall { .. })),
        "plain exec must not emit agent calls: {events:?}"
    );
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("agent is not defined"),
        "expected ReferenceError for missing `agent` global, got: {error_text}"
    );
}

/// Collect the first `count` [`RuntimeEvent::WorkflowCall`] events, skipping
/// the `Started`/`Pending` lifecycle events, and return their `(id, name,
/// args)` tuples in emission order.
async fn collect_workflow_calls(
    event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    count: usize,
) -> Vec<(String, String, Option<serde_json::Value>)> {
    let mut calls = Vec::new();
    while calls.len() < count {
        match next_event(event_rx).await {
            RuntimeEvent::WorkflowCall { id, name, args } => calls.push((id, name, args)),
            RuntimeEvent::Started | RuntimeEvent::Pending => {}
            other => panic!("unexpected event before workflow calls: {other:?}"),
        }
    }
    calls
}

#[tokio::test]
async fn workflow_call_emits_one_pending_promise_event_resolvable_by_id() {
    // `workflow('child')` returns a pending Promise and emits exactly one
    // WorkflowCall with a stamped id, the resolved name, and (absent second
    // arg) `args = None`. Responding by the emitted id resolves that promise,
    // proving the resolver is retrievable by id from `pending_tool_calls`.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request("text(await workflow('child'));"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let calls = collect_workflow_calls(&mut event_rx, 1).await;
    assert_eq!(calls.len(), 1);
    let (id, name, args) = &calls[0];
    assert_eq!(id, "workflow-0");
    assert_eq!(name, "child");
    assert_eq!(*args, None);

    runtime_tx
        .send(RuntimeCommand::ToolResponse {
            id: id.clone(),
            result: json!("child-result"),
        })
        .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    // The single WorkflowCall was already consumed before draining.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::WorkflowCall { .. }))
            .count(),
        0,
        "the single WorkflowCall was already consumed before draining: {events:?}"
    );
    let text = events.iter().find_map(|event| match event {
        RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
            Some(text.clone())
        }
        _ => None,
    });
    assert_eq!(text.as_deref(), Some("child-result"));
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(error_text.is_none(), "workflow body must run cleanly");
}

#[tokio::test]
async fn workflow_calls_carry_args_and_resolve_out_of_order() {
    // Two `workflow()` calls under Promise.all: each emits a distinct stamped
    // id and carries its args JSON verbatim. Responding in REVERSE arrival
    // order still resolves each promise by id (out-of-order safe, same
    // machinery as agent()/tool callbacks).
    let source = r#"
const results = await Promise.all([
  workflow('first', { n: 1 }),
  workflow('second', { n: 2, tags: ['x'] }),
]);
text(results.join(','));
"#;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_execute_request(source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let calls = collect_workflow_calls(&mut event_rx, 2).await;
    let ids: Vec<&str> = calls.iter().map(|(id, ..)| id.as_str()).collect();
    let names: Vec<&str> = calls.iter().map(|(_, name, _)| name.as_str()).collect();
    assert_eq!(ids, vec!["workflow-0", "workflow-1"]);
    assert_eq!(names, vec!["first", "second"]);
    assert_eq!(calls[0].2, Some(json!({ "n": 1 })));
    assert_eq!(calls[1].2, Some(json!({ "n": 2, "tags": ["x"] })));

    // Respond in reverse arrival order: workflow-1 then workflow-0.
    for (id, result) in [("workflow-1", json!("B")), ("workflow-0", json!("A"))] {
        runtime_tx
            .send(RuntimeCommand::ToolResponse {
                id: id.to_string(),
                result,
            })
            .unwrap();
    }

    let events = drain_to_result(&mut event_rx).await;
    let text = events.iter().find_map(|event| match event {
        RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
            Some(text.clone())
        }
        _ => None,
    });
    // Promise.all preserves source-order mapping despite reverse resolution.
    assert_eq!(text.as_deref(), Some("A,B"));
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    assert!(error_text.is_none(), "workflow body must run cleanly");
}

#[tokio::test]
async fn plain_exec_has_no_workflow_global() {
    // Workflow-ness is the explicit flag, never the source shape: a plain
    // exec calling `workflow()` sees a ReferenceError and emits no
    // WorkflowCall.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
        HashMap::new(),
        plain_execute_request("await workflow('child');"),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    let events = drain_to_result(&mut event_rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::WorkflowCall { .. })),
        "plain exec must not emit workflow calls: {events:?}"
    );
    let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
        panic!("last event must be Result");
    };
    let error_text = error_text.as_deref().unwrap_or_default();
    assert!(
        error_text.contains("workflow is not defined"),
        "expected ReferenceError for missing `workflow` global, got: {error_text}"
    );
}
