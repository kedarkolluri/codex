use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;

use super::HISTORY_OMISSION_MARKER;
use super::bound_workflow_child_compacted_history;
use super::validate_workflow_child_model_history;
use crate::context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES;
use crate::context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS;
use crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;

fn message(role: &str, text: impl Into<String>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn model_history_accepts_exact_limits_and_rejects_cap_plus_one() {
    let exact = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|_| message("user", "x"))
        .collect::<Vec<_>>();
    assert_eq!(validate_workflow_child_model_history(&exact), Ok(()));

    let mut too_many = exact;
    too_many.push(message("user", "overflow"));
    assert_eq!(
        validate_workflow_child_model_history(&too_many),
        Err("workflow child history exceeds model-context limits")
    );

    assert_eq!(
        validate_workflow_child_model_history(&[message(
            "user",
            "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1),
        )]),
        Err("workflow child history contains an oversized message")
    );
}

#[test]
fn model_history_rejects_agent_messages_and_aggregate_overflow() {
    let agent_message = ResponseItem::AgentMessage {
        id: None,
        author: "root".to_string(),
        recipient: "child".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "hidden".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    };
    assert_eq!(
        validate_workflow_child_model_history(&[agent_message]),
        Err("workflow child history contains inter-agent communication")
    );

    let aggregate = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|_| message("user", "x".repeat(2 * 1024)))
        .collect::<Vec<_>>();
    assert_eq!(
        validate_workflow_child_model_history(&aggregate),
        Err("workflow child history exceeds model-context limits")
    );
}

#[test]
fn model_generated_history_does_not_consume_injected_context_quotas() {
    let mut history = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS + 8)
        .map(|index| ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some(format!(
                "reasoning-{index}:{}",
                "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1)
            )),
            internal_chat_message_metadata_passthrough: None,
        })
        .collect::<Vec<_>>();
    history.push(ResponseItem::FunctionCall {
        id: None,
        name: "shell_command".to_string(),
        namespace: None,
        arguments: "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1),
        call_id: "model-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    history.push(ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });

    assert_eq!(validate_workflow_child_model_history(&history), Ok(()));
}

#[test]
fn model_generated_items_reset_contiguous_injected_segment_quotas() {
    let segment = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|_| message("user", "bounded"))
        .collect::<Vec<_>>();
    let mut history = segment.clone();
    history.push(ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content: None,
        encrypted_content: Some("model boundary".to_string()),
        internal_chat_message_metadata_passthrough: None,
    });
    history.extend(segment);

    assert_eq!(validate_workflow_child_model_history(&history), Ok(()));
}

#[test]
fn model_history_rejects_contiguous_tool_output_aggregate_overflow() {
    let outputs = (0..16)
        .map(|index| ResponseItem::FunctionCallOutput {
            id: None,
            call_id: format!("parallel-{index}"),
            output: FunctionCallOutputPayload::from_text("x".repeat(5 * 1024)),
            internal_chat_message_metadata_passthrough: None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        validate_workflow_child_model_history(&outputs),
        Err("workflow child history exceeds model-context limits")
    );
}

#[test]
fn model_history_rejects_legacy_assistant_encoded_inter_agent_communication() {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/workflow_child").expect("agent path"),
        Vec::new(),
        "hidden instruction".to_string(),
        /*trigger_turn*/ true,
    );
    let legacy_item: ResponseItem = communication.to_response_input_item().into();

    assert_eq!(
        validate_workflow_child_model_history(&[legacy_item]),
        Err("workflow child history contains inter-agent communication")
    );
}

#[test]
fn model_history_rejects_an_output_whose_immutable_wrapper_cannot_fit() {
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        output: FunctionCallOutputPayload::from_text("small".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        validate_workflow_child_model_history(&[output]),
        Err("workflow child history contains an oversized tool output")
    );
}

#[test]
fn compacted_history_bounds_utf8_count_and_aggregate_with_marker() {
    let items = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|index| {
            message(
                "user",
                format!("{index}:{}", "🦀".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES / 4)),
            )
        })
        .collect();

    let bounded = bound_workflow_child_compacted_history(items).expect("bound compaction");

    assert!(bounded.len() <= MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS);
    assert_eq!(validate_workflow_child_model_history(&bounded), Ok(()));
    assert!(format!("{bounded:?}").contains(HISTORY_OMISSION_MARKER));
    let bytes = bounded
        .iter()
        .map(|item| super::model_visible_item_usage(item).expect("valid item").1)
        .sum::<usize>();
    assert!(bytes <= MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES);
}

#[test]
fn compacted_history_preserves_a_bounded_semantic_tail() {
    let semantic_tail = ResponseItem::ContextCompaction {
        id: None,
        encrypted_content: Some("encrypted-summary".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut items = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|_| message("user", "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES)))
        .collect::<Vec<_>>();
    items.push(semantic_tail.clone());

    let bounded = bound_workflow_child_compacted_history(items).expect("bound compaction");

    assert_eq!(bounded.last(), Some(&semantic_tail));
    assert!(format!("{bounded:?}").contains(HISTORY_OMISSION_MARKER));
    assert_eq!(validate_workflow_child_model_history(&bounded), Ok(()));
}

#[test]
fn compacted_history_clears_untrusted_message_envelopes() {
    let item = ResponseItem::Message {
        id: Some(ResponseItemId::from_server(
            "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        )),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(
            InternalChatMessageMetadataPassthrough {
                turn_id: Some("x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES)),
            },
        ),
    };

    let bounded = bound_workflow_child_compacted_history(vec![item]).expect("bound compaction");

    let [ResponseItem::Message {
        id,
        internal_chat_message_metadata_passthrough,
        ..
    }] = bounded.as_slice()
    else {
        panic!("expected one compacted message");
    };
    assert_eq!(id, &None);
    assert_eq!(internal_chat_message_metadata_passthrough, &None);
    assert!(
        serde_json::to_vec(&bounded[0])
            .expect("serialize bounded message")
            .len()
            <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
}
