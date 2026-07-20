use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use pretty_assertions::assert_eq;

use super::WorkflowChildProgress;
use super::observe_event;

#[test]
fn event_tap_reports_token_and_tool_progress_as_it_arrives() {
    let token_usage = TokenUsage {
        input_tokens: 21,
        cached_input_tokens: 8,
        output_tokens: 13,
        reasoning_output_tokens: 5,
        total_tokens: 34,
    };
    let token_event = EventMsg::TokenCount(TokenCountEvent {
        info: Some(TokenUsageInfo {
            total_token_usage: token_usage.clone(),
            last_token_usage: token_usage.clone(),
            model_context_window: Some(128_000),
        }),
        rate_limits: None,
    });
    let mut progress = WorkflowChildProgress::default();

    assert!(observe_event(&mut progress, &token_event));
    assert_eq!(
        progress,
        WorkflowChildProgress {
            token_usage: token_usage.clone(),
            tool_call_count: 0,
        }
    );
    assert!(
        !observe_event(&mut progress, &token_event),
        "an identical cumulative token snapshot must not emit a duplicate update"
    );

    let tool_event = EventMsg::RawResponseItem(RawResponseItemEvent {
        item: ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "tool-call-1".to_string(),
            name: "lookup".to_string(),
            namespace: None,
            input: "{}".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    });
    assert!(observe_event(&mut progress, &tool_event));
    assert_eq!(
        progress,
        WorkflowChildProgress {
            token_usage,
            tool_call_count: 1,
        }
    );
}
