use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_ITEMS;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use super::CellId;
use super::CodeModeNestedToolCall;
use super::CodeModeSessionDelegate;
use super::InProcessCodeModeSession;
use super::NotificationFuture;
use super::RuntimeResponse;
use super::ToolInvocationFuture;
use super::protocol_cell_id;
use super::runtime_request;
use super::runtime_response;
use super::tests::cell_id;
use super::tests::execute;
use super::tests::execute_request;
use crate::CodeModeToolKind;
use crate::ExecuteOutputPolicy;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::ToolDefinition;
use crate::session_runtime as runtime;

async fn execute_saved(
    service: &InProcessCodeModeSession,
    request: ExecuteRequest,
) -> RuntimeResponse {
    let started = service
        .runtime
        .execute(
            runtime_request(request),
            runtime::ObserveMode::PendingFrontier,
        )
        .await
        .expect("start saved cell");
    let cell_id = protocol_cell_id(&started.cell_id);
    let event = started.initial_event().await.expect("saved cell event");
    runtime_response(&cell_id, event).expect("saved cell response")
}

fn saved_request(source: impl Into<String>) -> ExecuteRequest {
    ExecuteRequest {
        source: source.into(),
        output_policy: ExecuteOutputPolicy::SavedWorkflow,
        yield_time_ms: None,
        ..execute_request("")
    }
}

fn text_item(text: impl Into<String>) -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputText { text: text.into() }
}

#[derive(Default)]
struct RecordingToolDelegate {
    invocation_count: AtomicUsize,
}

impl CodeModeSessionDelegate for RecordingToolDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.invocation_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(JsonValue::Null) })
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

#[tokio::test]
async fn ordinary_output_remains_unbounded_by_saved_workflow_limits() {
    let service = InProcessCodeModeSession::new();
    let text = "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES);

    assert_eq!(
        execute(
            &service,
            ExecuteRequest {
                source: format!(r#"text("x".repeat({WORKFLOW_OUTPUT_ITEM_MAX_BYTES}));"#),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![text_item(text)],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn saved_workflow_output_rejection_stops_catch_and_nested_tool_invocation() {
    let delegate = Arc::new(RecordingToolDelegate::default());
    let service = InProcessCodeModeSession::with_delegate(delegate.clone());
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        execute_saved(
            &service,
            ExecuteRequest {
                enabled_tools: vec![ToolDefinition {
                    name: "gate".to_string(),
                    tool_name: ToolName::plain("gate"),
                    description: String::new(),
                    kind: CodeModeToolKind::Function,
                    input_schema: None,
                    output_schema: None,
                }],
                source: format!(
                    r#"
try {{
    text("x".repeat({WORKFLOW_OUTPUT_ITEM_MAX_BYTES}));
}} catch (_) {{}}
await tools.gate({{}});
"#
                ),
                ..saved_request("")
            },
        ),
    )
    .await
    .expect("saved-workflow output rejection must terminate the runtime");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
    assert_eq!(delegate.invocation_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn saved_workflow_syntax_errors_are_redacted() {
    let service = InProcessCodeModeSession::new();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        execute_saved(&service, saved_request("const private_syntax_detail = ;")),
    )
    .await
    .expect("saved-workflow syntax error response timeout");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        }
    );
}

#[tokio::test]
async fn saved_workflow_media_items_are_rejected_before_enqueue() {
    let service = InProcessCodeModeSession::new();
    let cases = [
        ("image", "data:image/png;base64,"),
        ("audio", "data:audio/wav;base64,"),
    ];

    for (index, (helper, prefix)) in cases.into_iter().enumerate() {
        let url_len = WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 1;
        let url = format!("{prefix}{}", "x".repeat(url_len - prefix.len()));
        let response =
            execute_saved(&service, saved_request(format!(r#"{helper}({url:?});"#))).await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id(&(index + 1).to_string()),
                content_items: Vec::new(),
                error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
            }
        );
    }
}

#[tokio::test]
async fn saved_workflow_generated_image_batch_is_transactional() {
    let service = InProcessCodeModeSession::new();
    let prefix_items = WORKFLOW_OUTPUT_MAX_ITEMS - 1;
    let source = format!(
        r#"
for (let index = 0; index < {prefix_items}; index += 1) {{
    text("");
}}
generatedImage({{
    image_url: "data:image/png;base64,YQ==",
    output_hint: "must not appear",
}});
"#
    );

    assert_eq!(
        execute_saved(&service, saved_request(source)).await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![text_item(""); prefix_items],
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
}

#[tokio::test]
async fn saved_workflow_notify_is_ordered_bounded_output() {
    let service = InProcessCodeModeSession::new();

    assert_eq!(
        execute_saved(
            &service,
            saved_request(r#"text("before"); notify("middle"); text("after");"#),
        )
        .await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![text_item("before"), text_item("middle"), text_item("after"),],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn saved_workflow_terminal_errors_are_bounded_and_redacted() {
    let service = InProcessCodeModeSession::new();

    assert_eq!(
        execute_saved(
            &service,
            saved_request(r#"throw new Error("private execution detail");"#),
        )
        .await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        }
    );
    assert_eq!(
        execute_saved(
            &service,
            saved_request(format!(
                r#"throw new Error("x".repeat({WORKFLOW_OUTPUT_ITEM_MAX_BYTES}));"#
            )),
        )
        .await,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
}
