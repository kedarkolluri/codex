use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use pretty_assertions::assert_eq;

use super::HISTORY_OMISSION_MARKER;
use super::MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES;
use super::WorkflowChildHistoryAppendMode;
use super::bound_workflow_child_compacted_history;
use super::finalize_workflow_child_history_append;
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

fn finalized_message_with_serialized_size(role: &str, serialized_size: usize) -> ResponseItem {
    let mut item = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix(
            "msg",
            "00000000-0000-7000-8000-000000000000",
        )),
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: String::new(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("00000000-0000-7000-8000-000000000000".to_string()),
        }),
    };
    let empty_size = serde_json::to_vec(&item)
        .expect("serialize empty finalized message")
        .len();
    assert!(serialized_size >= empty_size);
    let ResponseItem::Message { content, .. } = &mut item else {
        unreachable!("constructed a message")
    };
    content[0] = ContentItem::InputText {
        text: "x".repeat(serialized_size - empty_size),
    };
    assert_eq!(
        serde_json::to_vec(&item)
            .expect("serialize finalized boundary message")
            .len(),
        serialized_size
    );
    item
}

#[test]
fn finalized_appends_compose_and_only_trusted_prompt_consumes_reservation() {
    let mut history = Vec::new();
    for _ in 0..MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES / MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
        let context = finalized_message_with_serialized_size(
            "developer",
            MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        );
        let append = finalize_workflow_child_history_append(
            &history,
            vec![context.clone()],
            WorkflowChildHistoryAppendMode::Noncritical,
        )
        .expect("finalize context append");
        assert_eq!(append, vec![context]);
        history.extend(append);
    }
    assert_eq!(
        history
            .iter()
            .map(|item| serde_json::to_vec(item)
                .expect("serialize context item")
                .len())
            .sum::<usize>(),
        MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES
    );

    let spoofed_user =
        finalized_message_with_serialized_size("user", MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    assert_eq!(
        finalize_workflow_child_history_append(
            &history,
            vec![spoofed_user],
            WorkflowChildHistoryAppendMode::Noncritical,
        ),
        Ok(Vec::new())
    );

    let prompt =
        finalized_message_with_serialized_size("user", MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    let prompt_append = finalize_workflow_child_history_append(
        &history,
        vec![prompt.clone()],
        WorkflowChildHistoryAppendMode::ExactUserPrompt,
    )
    .expect("the trusted prompt owns the reserved slot");
    assert_eq!(prompt_append, vec![prompt.clone()]);
    history.extend(prompt_append);
    assert_eq!(
        history
            .iter()
            .map(|item| serde_json::to_vec(item)
                .expect("serialize finalized item")
                .len())
            .sum::<usize>(),
        MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
    );
    assert_eq!(validate_workflow_child_model_history(&history), Ok(()));
    assert_eq!(history.last(), Some(&prompt));

    assert_eq!(
        finalize_workflow_child_history_append(
            &history,
            vec![message("developer", "must remain outside the full segment")],
            WorkflowChildHistoryAppendMode::Noncritical,
        ),
        Ok(Vec::new())
    );
}

#[test]
fn semantic_compaction_tail_reserves_the_next_exact_prompt() {
    let mut history = vec![ResponseItem::ContextCompaction {
        id: Some(ResponseItemId::with_suffix("ctxc", "semantic-tail")),
        encrypted_content: Some("encrypted-summary".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }];
    for _ in 0..MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES / MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
        let context = finalized_message_with_serialized_size(
            "developer",
            MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        );
        let append = finalize_workflow_child_history_append(
            &history,
            vec![context.clone()],
            WorkflowChildHistoryAppendMode::Noncritical,
        )
        .expect("finalize post-compaction context");
        assert_eq!(append, vec![context]);
        history.extend(append);
    }
    assert_eq!(
        finalize_workflow_child_history_append(
            &history,
            vec![message(
                "developer",
                "must leave the prompt reservation intact"
            )],
            WorkflowChildHistoryAppendMode::Noncritical,
        ),
        Ok(Vec::new())
    );

    let prompt =
        finalized_message_with_serialized_size("user", MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    let append = finalize_workflow_child_history_append(
        &history,
        vec![prompt.clone()],
        WorkflowChildHistoryAppendMode::ExactUserPrompt,
    )
    .expect("exact prompt should fit after semantic compaction context");

    assert_eq!(append, vec![prompt]);
    history.extend(append);
    assert_eq!(validate_workflow_child_model_history(&history), Ok(()));
}

#[test]
fn finalized_message_preserves_envelope_while_bounding_escaped_utf8_payload() {
    let id = ResponseItemId::with_suffix("msg", "trusted-id");
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("trusted-turn".to_string()),
    };
    let item = ResponseItem::Message {
        id: Some(id.clone()),
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "\0é".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(metadata.clone()),
    };

    let finalized = finalize_workflow_child_history_append(
        &[],
        vec![item],
        WorkflowChildHistoryAppendMode::Noncritical,
    )
    .expect("finalize escaped UTF-8 message");

    let [
        ResponseItem::Message {
            id: finalized_id,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        },
    ] = finalized.as_slice()
    else {
        panic!("expected one finalized message");
    };
    assert_eq!(finalized_id, &Some(id));
    assert_eq!(internal_chat_message_metadata_passthrough, &Some(metadata));
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one finalized text fragment");
    };
    assert!(text.ends_with(super::FINALIZED_CONTEXT_TRUNCATION_MARKER));
    assert!(text.is_char_boundary(text.len()));
    assert!(
        serde_json::to_vec(&finalized[0])
            .expect("serialize finalized escaped UTF-8 message")
            .len()
            <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
}

#[test]
fn finalized_append_never_rewrites_model_generated_output() {
    let assistant = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "assistant-output")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "model output".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("assistant-turn".to_string()),
        }),
    };

    assert_eq!(
        finalize_workflow_child_history_append(
            &[],
            vec![assistant.clone()],
            WorkflowChildHistoryAppendMode::Noncritical,
        ),
        Ok(vec![assistant])
    );
}

#[test]
fn finalized_output_uses_bounded_omission_to_preserve_call_pairing() {
    let call_id = "paired-call".to_string();
    let mut history = vec![ResponseItem::FunctionCall {
        id: Some(ResponseItemId::with_suffix("fc", "paired")),
        name: "shell_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.clone(),
        internal_chat_message_metadata_passthrough: None,
    }];
    for serialized_size in [
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES - 1024,
    ] {
        history.push(finalized_message_with_serialized_size(
            "developer",
            serialized_size,
        ));
    }
    let output = ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::with_suffix("fco", "paired")),
        call_id,
        output: FunctionCallOutputPayload::from_text(
            "oversized output".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        ),
        internal_chat_message_metadata_passthrough: None,
    };
    let expected =
        super::omitted_tool_output(super::bound_workflow_child_output_item(output.clone()));

    let finalized = finalize_workflow_child_history_append(
        &history,
        vec![output],
        WorkflowChildHistoryAppendMode::Noncritical,
    )
    .expect("finalize paired output");

    assert_eq!(finalized, vec![expected]);
    let mut completed_history = history;
    completed_history.extend(finalized);
    assert_eq!(
        validate_workflow_child_model_history(&completed_history),
        Ok(())
    );
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
fn model_history_aggregate_charges_full_serialized_message_envelopes() {
    let history = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|_| ResponseItem::Message {
            id: Some(ResponseItemId::from_server("x".repeat(2 * 1024))),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "x".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
        .collect::<Vec<_>>();

    assert!(
        history.iter().all(|item| {
            serde_json::to_vec(item)
                .expect("serialize history item")
                .len()
                <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
        }),
        "the aggregate guard, not the per-item guard, must reject this history"
    );
    assert_eq!(
        validate_workflow_child_model_history(&history),
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
                format!(
                    "{index}:{}",
                    "🦀".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES / 4)
                ),
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
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES)),
        }),
    };

    let bounded = bound_workflow_child_compacted_history(vec![item]).expect("bound compaction");

    let [
        ResponseItem::Message {
            id,
            internal_chat_message_metadata_passthrough,
            ..
        },
    ] = bounded.as_slice()
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
