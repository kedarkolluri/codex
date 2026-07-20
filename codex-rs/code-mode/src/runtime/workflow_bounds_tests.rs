use std::collections::HashMap;
use std::time::Duration;

use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::WORKFLOW_AGENT_MAX_RETRIES;
use codex_code_mode_protocol::WORKFLOW_LOG_MAX_EVENTS;
use codex_code_mode_protocol::WORKFLOW_PHASE_MAX_EVENTS;
use codex_code_mode_protocol::WORKFLOW_TOPOLOGY_MAX_NODES;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use super::PendingRuntimeMode;
use super::RuntimeEvent;
use super::spawn_runtime;

#[derive(Clone, Copy)]
enum ForbiddenDispatch {
    Agent,
    Workflow,
    Phase,
    Log,
}

#[test]
fn journal_record_cap_covers_every_admitted_workflow_record() {
    let attempts_per_agent =
        usize::try_from(WORKFLOW_AGENT_MAX_RETRIES).expect("retry cap fits usize") + 1;
    let topology_nodes =
        usize::try_from(WORKFLOW_TOPOLOGY_MAX_NODES).expect("topology cap fits usize");
    let workflow_logs = usize::try_from(WORKFLOW_LOG_MAX_EVENTS).expect("log cap fits usize");
    let workflow_phases = usize::try_from(WORKFLOW_PHASE_MAX_EVENTS).expect("phase cap fits usize");

    // Each attempt can persist one child binding and one worktree-cleanup diagnostic. Each
    // logical agent then persists exactly one terminal record, in addition to the run header and
    // workflow-authored log/phase records.
    let admitted_records =
        1 + topology_nodes * (attempts_per_agent * 2 + 1) + workflow_logs + workflow_phases;

    assert_eq!(admitted_records, 57_001);
    assert!(admitted_records <= codex_workflow_journal::WORKFLOW_JOURNAL_MAX_RECORDS);
}

#[tokio::test]
async fn workflow_authored_context_is_bounded_before_host_dispatch() {
    let cases = [
        (
            "await agent('x'.repeat(8193));",
            "workflow agent prompt exceeds the 8192-byte limit",
            ForbiddenDispatch::Agent,
        ),
        (
            "await agent('ok', { label: 'x'.repeat(513) });",
            "workflow agent label exceeds the 512-byte limit",
            ForbiddenDispatch::Agent,
        ),
        (
            "await agent('ok', { schema: { description: 'x'.repeat(32769) } });",
            "opts.schema is too large",
            ForbiddenDispatch::Agent,
        ),
        (
            "let schema = {}; for (let i = 0; i < 65; i++) schema = { items: schema }; await agent('ok', { schema });",
            "opts.schema nesting is too deep",
            ForbiddenDispatch::Agent,
        ),
        (
            "await workflow('nested', { value: 'x'.repeat(32768) });",
            "workflow args exceed the 32768-byte execution cap",
            ForbiddenDispatch::Workflow,
        ),
        (
            "phase('x'.repeat(513));",
            "workflow phase title exceeds the 512-byte limit",
            ForbiddenDispatch::Phase,
        ),
        (
            "log('x'.repeat(4097));",
            "workflow log message exceeds the 4096-byte limit",
            ForbiddenDispatch::Log,
        ),
    ];

    for (source, expected_error, forbidden_dispatch) in cases {
        let events = execute_workflow(source).await;
        let error = terminal_error(&events);
        assert!(
            error.contains(expected_error),
            "unexpected error for `{source}`: {error}"
        );
        assert_eq!(
            events
                .iter()
                .any(|event| is_forbidden_dispatch(event, forbidden_dispatch)),
            false,
            "`{source}` crossed the host dispatch boundary"
        );
    }
}

#[tokio::test]
async fn workflow_text_output_is_bounded_before_nested_result_aggregation() {
    let cases = [
        (
            "text('x'.repeat(32769));",
            "workflow output byte cap exceeded",
        ),
        (
            "text(String.fromCharCode(0).repeat(6000));",
            "workflow output byte cap exceeded",
        ),
        (
            "text('x'.repeat(20000)); text('y'.repeat(20000));",
            "workflow output byte cap exceeded",
        ),
        (
            "for (let i = 0; i < 257; i++) text('x');",
            "workflow output item cap exceeded",
        ),
    ];

    for (source, expected_error) in cases {
        let events = execute_workflow(source).await;
        let error = terminal_error(&events);
        assert!(
            error.contains(expected_error),
            "unexpected error for `{source}`: {error}"
        );
        let output = events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                    Some(text)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(output.len() <= codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_ITEMS);
        let serialized_bytes = output
            .iter()
            .map(|text| {
                serde_json::to_vec(text)
                    .expect("serialize output segment")
                    .len()
            })
            .sum::<usize>();
        assert!(serialized_bytes <= codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_BYTES);
    }
}

#[tokio::test]
async fn workflow_image_and_generated_image_output_is_bounded_before_emission() {
    for source in [
        "image('data:image/png;base64,' + 'x'.repeat(32768));",
        "generatedImage({ image_url: 'data:image/png;base64,AAA', output_hint: 'x'.repeat(32768) });",
    ] {
        let events = execute_workflow(source).await;
        let error = terminal_error(&events);
        assert!(
            error.contains("workflow output byte cap exceeded"),
            "unexpected error for `{source}`: {error}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ContentItem(_))),
            "a rejected image chunk must not be partially emitted: {events:?}"
        );
    }

    let events =
        execute_workflow("for (let i = 0; i < 257; i++) image('data:image/png;base64,AAA');").await;
    assert!(
        terminal_error(&events).contains("workflow output item cap exceeded"),
        "unexpected item-cap result: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ContentItem(_)))
            .count(),
        codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_ITEMS
    );
}

async fn execute_workflow(source: &str) -> Vec<RuntimeEvent> {
    let request = ExecuteRequest {
        tool_call_id: "workflow-bounds".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        workflow: true,
        args: None,
        run_id: Some("run-workflow-bounds".to_string()),
        replay_entries: Vec::new(),
        workflow_budget: None,
    };
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (_runtime_tx, _control_tx, _terminate_handle) = spawn_runtime(
        HashMap::new(),
        request,
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .expect("workflow runtime should start");

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("workflow runtime event timeout")
            .expect("workflow runtime event stream closed before its result");
        let is_result = matches!(event, RuntimeEvent::Result { .. });
        events.push(event);
        if is_result {
            return events;
        }
    }
}

fn terminal_error(events: &[RuntimeEvent]) -> &str {
    let Some(RuntimeEvent::Result {
        error_text: Some(error),
        ..
    }) = events.last()
    else {
        panic!("workflow must end with an error result");
    };
    error
}

fn is_forbidden_dispatch(event: &RuntimeEvent, forbidden: ForbiddenDispatch) -> bool {
    matches!(
        (event, forbidden),
        (RuntimeEvent::AgentCall { .. }, ForbiddenDispatch::Agent)
            | (
                RuntimeEvent::WorkflowCall { .. },
                ForbiddenDispatch::Workflow
            )
            | (RuntimeEvent::Phase { .. }, ForbiddenDispatch::Phase)
            | (RuntimeEvent::WorkflowLog { .. }, ForbiddenDispatch::Log)
    )
}
