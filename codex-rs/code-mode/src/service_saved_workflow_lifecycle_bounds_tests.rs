use std::sync::Arc;
use std::time::Duration;

use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_BYTES;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::CellId;
use super::CodeModeNestedToolCall;
use super::CodeModeSessionDelegate;
use super::InProcessCodeModeSession;
use super::NotificationFuture;
use super::ToolInvocationFuture;
use super::runtime_request;
use super::tests::execute_request;
use crate::CodeModeToolKind;
use crate::ExecuteOutputPolicy;
use crate::ExecuteRequest;
use crate::ToolDefinition;
use crate::session_runtime as runtime;

const PRIVATE_TOOL_ERROR: &str = "private nested tool failure";
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum HeldToolCompletion {
    Success,
    Error(&'static str),
}

struct HeldToolDelegate {
    release: Notify,
    started: Notify,
    completion: HeldToolCompletion,
}

impl Default for HeldToolDelegate {
    fn default() -> Self {
        Self {
            release: Notify::new(),
            started: Notify::new(),
            completion: HeldToolCompletion::Success,
        }
    }
}

impl HeldToolDelegate {
    fn failing(error_text: &'static str) -> Self {
        Self {
            release: Notify::new(),
            started: Notify::new(),
            completion: HeldToolCompletion::Error(error_text),
        }
    }

    async fn wait_until_started(&self) {
        tokio::time::timeout(TEST_TIMEOUT, self.started.notified())
            .await
            .expect("saved workflow tool start timeout");
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

impl CodeModeSessionDelegate for HeldToolDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.started.notify_one();
        Box::pin(async move {
            tokio::select! {
                _ = self.release.notified() => match self.completion {
                    HeldToolCompletion::Success => Ok(JsonValue::Null),
                    HeldToolCompletion::Error(error_text) => Err(error_text.to_string()),
                },
                _ = cancellation_token.cancelled() => Err("cancelled".to_string()),
            }
        })
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

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn held_tool() -> ToolDefinition {
    ToolDefinition {
        name: "gate".to_string(),
        tool_name: ToolName::plain("gate"),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }
}

fn saved_request(source: impl Into<String>) -> ExecuteRequest {
    ExecuteRequest {
        enabled_tools: vec![held_tool()],
        source: source.into(),
        output_policy: ExecuteOutputPolicy::SavedWorkflow,
        yield_time_ms: None,
        ..execute_request("")
    }
}

async fn execute_through_pending(
    service: &InProcessCodeModeSession,
    delegate: &HeldToolDelegate,
    request: ExecuteRequest,
) -> (runtime::CellEvent, runtime::CellEvent) {
    let started = tokio::time::timeout(
        TEST_TIMEOUT,
        service.runtime.execute(
            runtime_request(request),
            runtime::ObserveMode::PendingFrontier,
        ),
    )
    .await
    .expect("start saved cell timeout")
    .expect("start saved cell");
    let runtime_cell_id = started.cell_id.clone();
    let pending = tokio::time::timeout(TEST_TIMEOUT, started.initial_event())
        .await
        .expect("initial saved event timeout")
        .expect("initial saved event");
    delegate.wait_until_started().await;
    let terminal = tokio::time::timeout(
        TEST_TIMEOUT,
        service.runtime.begin_observe(
            &runtime_cell_id,
            runtime::ObserveMode::YieldAfter(TEST_TIMEOUT),
        ),
    )
    .await
    .expect("observe held saved cell timeout")
    .expect("observe held saved cell");
    delegate.release();
    let terminal = tokio::time::timeout(TEST_TIMEOUT, terminal.event())
        .await
        .expect("terminal saved event timeout")
        .expect("terminal saved event");
    (pending, terminal)
}

#[tokio::test]
async fn saved_workflow_budget_survives_pending_observation_boundary() {
    let delegate = Arc::new(HeldToolDelegate::default());
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let exact_item_len = WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2;
    let item_count = WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
    let request = saved_request(format!(
        r#"
for (let index = 0; index < {item_count}; index += 1) {{
    text("x".repeat({exact_item_len}));
}}
await tools.gate({{}});
text("late payload");
throw new Error("private late failure");
"#
    ));
    let exact_text = "x".repeat(exact_item_len);

    let (pending, terminal) = execute_through_pending(&service, &delegate, request).await;

    assert_eq!(
        pending,
        runtime::CellEvent::Pending {
            content_items: vec![runtime::OutputItem::Text { text: exact_text }; item_count],
            pending_tool_call_ids: vec!["tool-1".to_string()],
        }
    );
    assert_eq!(
        terminal,
        runtime::CellEvent::Completed {
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
}

#[tokio::test]
async fn saved_workflow_pending_tool_error_is_redacted_with_available_budget() {
    let delegate = Arc::new(HeldToolDelegate::failing(PRIVATE_TOOL_ERROR));
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());

    let (pending, terminal) =
        execute_through_pending(&service, &delegate, saved_request("await tools.gate({});")).await;

    assert_eq!(
        pending,
        runtime::CellEvent::Pending {
            content_items: Vec::new(),
            pending_tool_call_ids: vec!["tool-1".to_string()],
        }
    );
    assert_eq!(
        terminal,
        runtime::CellEvent::Completed {
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        }
    );
}

#[tokio::test]
async fn saved_workflow_pending_tool_error_is_rejected_with_one_aggregate_byte_left() {
    let delegate = Arc::new(HeldToolDelegate::failing(PRIVATE_TOOL_ERROR));
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let exact_item_len = WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2;
    let exact_item_count = WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 1;
    let final_serialized_len =
        WORKFLOW_OUTPUT_MAX_BYTES - exact_item_count * WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 1;
    let final_item_len = final_serialized_len - 2;
    let request = saved_request(format!(
        r#"
for (let index = 0; index < {exact_item_count}; index += 1) {{
    text("x".repeat({exact_item_len}));
}}
text("x".repeat({final_item_len}));
await tools.gate({{}});
"#
    ));
    let mut expected_items = vec![
        runtime::OutputItem::Text {
            text: "x".repeat(exact_item_len),
        };
        exact_item_count
    ];
    expected_items.push(runtime::OutputItem::Text {
        text: "x".repeat(final_item_len),
    });

    let (pending, terminal) = execute_through_pending(&service, &delegate, request).await;

    assert_eq!(
        pending,
        runtime::CellEvent::Pending {
            content_items: expected_items,
            pending_tool_call_ids: vec!["tool-1".to_string()],
        }
    );
    assert_eq!(
        terminal,
        runtime::CellEvent::Completed {
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
}
