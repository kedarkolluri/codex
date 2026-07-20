use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::ResponseItemId;
use pretty_assertions::assert_eq;

use crate::config::MultiAgentV2Config;
use super::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;
use super::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES;
use super::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS;
use super::TRUNCATION_MARKER;
use super::bound_workflow_child_context_items;
use super::bound_workflow_child_injected_messages;
use super::validate_workflow_child_client_injected_items;

fn message(role: &str, content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn splits_and_bounds_every_text_fragment() {
    let oversized = format!("<opening>{}</closing>", "🦀".repeat(4_000));
    let items = bound_workflow_child_context_items(vec![message(
        "developer",
        vec![
            ContentItem::InputText {
                text: "short".to_string(),
            },
            ContentItem::InputText {
                text: oversized.clone(),
            },
        ],
    )]);

    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0],
        message(
            "developer",
            vec![ContentItem::InputText {
                text: "short".to_string(),
            }],
        )
    );
    let ResponseItem::Message { role, content, .. } = &items[1] else {
        panic!("expected a bounded message");
    };
    assert!(
        serde_json::to_vec(&items[1])
            .expect("serialize bounded item")
            .len()
            <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
    assert_eq!(role, "developer");
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one bounded text fragment");
    };
    assert!(text.len() <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    assert!(text.starts_with("<opening>"));
    assert!(text.ends_with("</closing>"));
    assert!(text.contains(TRUNCATION_MARKER));
    assert!(text.is_char_boundary(text.len()));
    assert_ne!(text, &oversized);
}

#[test]
fn exact_byte_cap_is_preserved_and_cap_plus_one_is_truncated() {
    let exact = "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    let over = format!("OPEN{}CLOSE", "é".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES));

    assert_eq!(super::bound_text(exact.clone()), exact);
    let bounded = super::bound_text(over);
    assert!(bounded.len() <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    assert!(bounded.starts_with("OPEN"));
    assert!(bounded.ends_with("CLOSE"));
    assert!(bounded.contains(TRUNCATION_MARKER));
}

#[test]
fn split_clears_untrusted_envelope_and_preserves_phase() {
    let original_id = ResponseItemId::from_server(
        "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
    );
    let metadata = InternalChatMessageMetadataPassthrough {
        turn_id: Some("x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES)),
    };
    let items = bound_workflow_child_context_items(vec![ResponseItem::Message {
        id: Some(original_id.clone()),
        role: "developer".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "first".to_string(),
            },
            ContentItem::InputText {
                text: "second".to_string(),
            },
        ],
        phase: Some(MessagePhase::Commentary),
        internal_chat_message_metadata_passthrough: Some(metadata.clone()),
    }]);

    assert_eq!(
        items,
        vec![
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "first".to_string(),
                }],
                phase: Some(MessagePhase::Commentary),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "second".to_string(),
                }],
                phase: Some(MessagePhase::Commentary),
                internal_chat_message_metadata_passthrough: None,
            },
        ]
    );
    assert!(items.iter().all(|item| {
        serde_json::to_vec(item).expect("serialize bounded item").len()
            <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    }));
}

#[test]
fn lowers_unknown_roles_and_omits_stale_multi_agent_guidance() {
    let stale_mode = format!(
        "{MULTI_AGENT_MODE_OPEN_TAG}spawn agents{MULTI_AGENT_MODE_CLOSE_TAG}"
    );
    let stale_hint =
        "<multi_agent_usage_hint>\nspawn agents\n</multi_agent_usage_hint>".to_string();
    let legacy_hint = MultiAgentV2Config::default()
        .root_agent_usage_hint_text
        .expect("default legacy root hint");
    let items = bound_workflow_child_context_items(vec![message(
        "system",
        vec![
            ContentItem::InputText { text: stale_mode },
            ContentItem::InputText { text: stale_hint },
            ContentItem::InputText { text: legacy_hint },
            ContentItem::OutputText {
                text: "safe context".to_string(),
            },
        ],
    )]);

    assert_eq!(
        items,
        vec![message(
            "user",
            vec![ContentItem::InputText {
                text: "safe context".to_string(),
            }],
        )]
    );
}

#[test]
fn discards_non_text_context() {
    let items = bound_workflow_child_context_items(vec![message(
        "user",
        vec![
            ContentItem::InputText {
                text: "context".to_string(),
            },
            ContentItem::InputImage {
                image_url: "data:image/png;base64,AA==".to_string(),
                detail: None,
            },
        ],
    )]);

    assert_eq!(
        items,
        vec![message(
            "user",
            vec![ContentItem::InputText {
                text: "context".to_string(),
            }],
        )]
    );
}

#[test]
fn batch_count_limit_is_deterministic_and_includes_an_omission_marker() {
    let items = (0..=MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|index| {
            message(
                "user",
                vec![ContentItem::InputText {
                    text: format!("context-{index}"),
                }],
            )
        })
        .collect();

    let bounded = bound_workflow_child_context_items(items);

    assert_eq!(bounded.len(), MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS);
    assert_eq!(
        bounded.first(),
        Some(&message(
            "user",
            vec![ContentItem::InputText {
                text: "context-0".to_string(),
            }],
        ))
    );
    let total_bytes = bounded
        .iter()
        .map(|item| serde_json::to_vec(item).expect("serialize item").len())
        .sum::<usize>();
    assert!(total_bytes <= MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES);
    assert!(format!("{bounded:?}").contains(super::BATCH_OMISSION_MARKER));
}

#[test]
fn batch_byte_limit_is_deterministic_for_multibyte_text() {
    let items = (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
        .map(|index| {
            message(
                "developer",
                vec![ContentItem::InputText {
                    text: format!("{index}:{}", "🦀".repeat(2_048)),
                }],
            )
        })
        .collect();

    let bounded = bound_workflow_child_context_items(items);
    let total_bytes = bounded
        .iter()
        .map(|item| serde_json::to_vec(item).expect("serialize item").len())
        .sum::<usize>();

    assert!(total_bytes <= MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES);
    assert!(bounded.len() < MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS);
    assert!(format!("{bounded:?}").contains(super::BATCH_OMISSION_MARKER));
}

#[test]
fn client_injection_accepts_only_a_bounded_batch_of_text_messages() {
    let exact = message(
        "user",
        (0..MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS)
            .map(|index| ContentItem::InputText {
                text: format!("context-{index}"),
            })
            .collect(),
    );
    assert_eq!(
        validate_workflow_child_client_injected_items(&[exact.clone()]),
        Ok(())
    );

    let ResponseItem::Message { mut content, .. } = exact else {
        panic!("expected message");
    };
    content.push(ContentItem::InputText {
        text: "overflow".to_string(),
    });
    assert_eq!(
        validate_workflow_child_client_injected_items(&[message("user", content)]),
        Err("workflow-managed thread injection exceeds the 64-item limit")
    );
    assert_eq!(
        validate_workflow_child_client_injected_items(&[ResponseItem::Other]),
        Err("workflow-managed threads accept only text message injection")
    );
    assert_eq!(
        validate_workflow_child_client_injected_items(&[message(
            "user",
            vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,AA==".to_string(),
                detail: None,
            }],
        )]),
        Err("workflow-managed threads accept only text message injection")
    );
}

#[test]
fn generic_injection_discards_unsupported_protocol_items() {
    assert_eq!(
        bound_workflow_child_injected_messages(vec![
            ResponseItem::Other,
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "notify-call".to_string(),
                name: Some("exec".to_string()),
                output: FunctionCallOutputPayload::from_text("notify payload".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]),
        Vec::<ResponseItem>::new()
    );
}
