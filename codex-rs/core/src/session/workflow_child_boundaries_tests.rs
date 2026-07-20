use std::sync::Arc;

use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

use super::Session;
use super::tests::make_session_and_context_with_rx;
use super::tests::make_workflow_session_and_context_with_rx;
use crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;
use crate::context::MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES;

fn message(role: &str, text: impl Into<String>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn message_with_serialized_size(serialized_size: usize) -> ResponseItem {
    let empty = message("user", "");
    let empty_size = serde_json::to_vec(&empty)
        .expect("serialize empty boundary message")
        .len();
    let item = message("user", "x".repeat(serialized_size - empty_size));
    assert_eq!(
        serde_json::to_vec(&item)
            .expect("serialize boundary message")
            .len(),
        serialized_size
    );
    item
}

fn agent_message() -> ResponseItem {
    ResponseItem::AgentMessage {
        id: None,
        author: "root".to_string(),
        recipient: "workflow-child".to_string(),
        content: vec![AgentMessageInputContent::InputText {
            text: "must not enter workflow child history".to_string(),
        }],
        internal_chat_message_metadata_passthrough: None,
    }
}

fn incomplete_call_with_oversized_id() -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "shell_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn resumed_history(session: &Session, items: Vec<ResponseItem>) -> InitialHistory {
    InitialHistory::Resumed(ResumedHistory {
        conversation_id: session.thread_id(),
        history: Arc::new(items.into_iter().map(RolloutItem::ResponseItem).collect()),
        rollout_path: None,
    })
}

fn compacted_item() -> CompactedItem {
    CompactedItem {
        message: "replacement summary".to_string(),
        replacement_history: None,
        window_number: Some(99),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }
}

#[tokio::test]
async fn workflow_resume_rejects_unsafe_active_history_without_installing_it() {
    let (session, turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    let original_history = vec![message("user", "safe active history")];
    session
        .replace_history(
            original_history.clone(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    let original_reference_context =
        serde_json::to_value(session.reference_context_item().await).expect("serialize context");
    let original_window = session.current_window_id().await;

    let unsafe_items = [
        agent_message(),
        message(
            "user",
            "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1),
        ),
        incomplete_call_with_oversized_id(),
    ];
    for unsafe_item in unsafe_items {
        let error = session
            .try_record_initial_history(resumed_history(session.as_ref(), vec![unsafe_item]))
            .await
            .expect_err("unsafe workflow child history must fail closed");

        assert!(matches!(error, CodexErr::InvalidRequest(_)));
        assert_eq!(
            session.clone_history().await.raw_items(),
            original_history.as_slice()
        );
        assert_eq!(session.current_window_id().await, original_window);
        assert_eq!(
            serde_json::to_value(session.reference_context_item().await)
                .expect("serialize unchanged context"),
            original_reference_context
        );
    }
}

#[tokio::test]
async fn ordinary_resume_preserves_the_same_history_items() {
    let (session, _turn_context, _events) = make_session_and_context_with_rx().await;
    let items = vec![
        agent_message(),
        message(
            "user",
            "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES + 1),
        ),
    ];

    session
        .try_record_initial_history(resumed_history(session.as_ref(), items.clone()))
        .await
        .expect("ordinary resume must retain its existing behavior");

    assert_eq!(session.clone_history().await.raw_items(), items.as_slice());
}

#[tokio::test]
async fn rejected_workflow_compaction_is_atomic_for_history_context_and_window() {
    let (session, turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    let original_history = vec![message("user", "safe pre-compaction history")];
    session
        .replace_history(
            original_history.clone(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    let original_reference_context =
        serde_json::to_value(session.reference_context_item().await).expect("serialize context");
    let original_window = session.current_window_id().await;
    let original_window_ids = session.state.lock().await.auto_compact_window_ids();
    let mut rejected_reference_context = turn_context.to_turn_context_item();
    rejected_reference_context.model = "must-not-be-installed".to_string();

    let error = session
        .replace_compacted_history(
            turn_context.as_ref(),
            vec![
                message("user", "locally processed candidate"),
                agent_message(),
            ],
            Some(rejected_reference_context),
            /*world_state_baseline*/ None,
            compacted_item(),
        )
        .await
        .expect_err("unsupported workflow compaction output must be rejected");

    assert!(matches!(error, CodexErr::InvalidRequest(_)));
    assert_eq!(
        session.clone_history().await.raw_items(),
        original_history.as_slice()
    );
    assert_eq!(session.current_window_id().await, original_window);
    assert_eq!(
        session.state.lock().await.auto_compact_window_ids(),
        original_window_ids
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize unchanged context"),
        original_reference_context
    );
}

#[tokio::test]
async fn workflow_compaction_preparation_rejects_post_id_overflow_before_window_advance() {
    let (session, mut turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should not be shared")
        .history_mode = ThreadHistoryMode::Paginated;
    let candidate = message_with_serialized_size(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    let bounded_without_id =
        crate::context::bound_workflow_child_compacted_history(vec![candidate.clone()])
            .expect("the pre-ID item is exactly within the limit");
    assert_eq!(bounded_without_id[0].id(), None);
    assert_eq!(
        serde_json::to_vec(&bounded_without_id[0])
            .expect("serialize pre-ID item")
            .len(),
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
    let original_history = session.clone_history().await.raw_items().to_vec();
    let original_window = session.current_window_id().await;
    let original_window_ids = session.state.lock().await.auto_compact_window_ids();

    let error = session
        .prepare_compacted_history_for_install(turn_context.as_ref(), vec![candidate])
        .await
        .expect_err("the final item ID must participate in exact validation");

    assert_eq!(
        error.to_string(),
        "workflow child history contains an oversized message"
    );
    assert_eq!(
        session.clone_history().await.raw_items(),
        original_history.as_slice()
    );
    assert_eq!(session.current_window_id().await, original_window);
    assert_eq!(
        session.state.lock().await.auto_compact_window_ids(),
        original_window_ids
    );
}

#[tokio::test]
async fn post_id_compaction_rejection_is_atomic_at_install_boundary() {
    let (session, mut turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should not be shared")
        .history_mode = ThreadHistoryMode::Paginated;
    let original_history = vec![message("user", "safe pre-compaction history")];
    session
        .replace_history(
            original_history.clone(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    let original_reference_context =
        serde_json::to_value(session.reference_context_item().await).expect("serialize context");
    let original_window = session.current_window_id().await;
    let original_window_ids = session.state.lock().await.auto_compact_window_ids();
    let mut rejected_reference_context = turn_context.to_turn_context_item();
    rejected_reference_context.model = "must-not-be-installed".to_string();

    let error = session
        .replace_compacted_history(
            turn_context.as_ref(),
            vec![message_with_serialized_size(
                MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES,
            )],
            Some(rejected_reference_context),
            /*world_state_baseline*/ None,
            compacted_item(),
        )
        .await
        .expect_err("post-ID overflow must fail before compaction installation");

    assert_eq!(
        error.to_string(),
        "workflow child history contains an oversized message"
    );
    assert_eq!(
        session.clone_history().await.raw_items(),
        original_history.as_slice()
    );
    assert_eq!(session.current_window_id().await, original_window);
    assert_eq!(
        session.state.lock().await.auto_compact_window_ids(),
        original_window_ids
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize unchanged context"),
        original_reference_context
    );
}

#[tokio::test]
async fn workflow_mailbox_communication_is_discarded_before_enqueue() {
    let (session, _turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/workflow_child").expect("agent path"),
        Vec::new(),
        "must not enter the workflow child mailbox".to_string(),
        /*trigger_turn*/ true,
    );

    super::handlers::inter_agent_communication(&session, "mailbox-test".to_string(), communication)
        .await;

    assert!(!session.input_queue.has_pending_mailbox_items().await);
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn central_submission_fence_rejects_workflow_child_mutation() {
    let (session, _turn_context, events) = make_workflow_session_and_context_with_rx().await;
    let operation = super::handlers::workflow_managed_restricted_operation(&Op::Compact)
        .expect("manual compaction must be restricted");
    let guardian_event = serde_json::from_value(serde_json::json!({
        "id": "guardian-review",
        "turn_id": "guardian-turn",
        "status": "denied",
        "action": {
            "type": "network_access",
            "target": "https://example.com",
            "host": "example.com",
            "protocol": "https",
            "port": 443
        }
    }))
    .expect("guardian denial event");
    assert_eq!(
        super::handlers::workflow_managed_restricted_operation(&Op::ApproveGuardianDeniedAction {
            event: guardian_event,
        },),
        Some("Guardian denied-action approval")
    );
    assert_eq!(
        super::handlers::workflow_managed_restricted_operation(&Op::RefreshMcpServers {
            config: codex_protocol::protocol::McpServerRefreshConfig {
                mcp_servers: serde_json::Value::Null,
                mcp_oauth_credentials_store_mode: serde_json::Value::Null,
                auth_keyring_backend_kind: serde_json::Value::Null,
            },
        }),
        Some("MCP server refresh")
    );
    assert_eq!(
        super::handlers::workflow_managed_restricted_operation(&Op::ReloadUserConfig),
        Some("user config reload")
    );

    assert!(
        super::handlers::reject_workflow_managed_operation(
            session.as_ref(),
            "restricted-op".to_string(),
            operation,
        )
        .await
    );

    let event = events.recv().await.expect("workflow mutation error event");
    let EventMsg::Error(error) = event.msg else {
        panic!("expected workflow mutation error");
    };
    assert_eq!(event.id, "restricted-op");
    assert_eq!(
        (error.message.as_str(), error.codex_error_info),
        (
            "workflow-managed threads do not support manual compaction",
            Some(CodexErrorInfo::BadRequest),
        )
    );

    let (ordinary_session, _turn_context, _events) = make_session_and_context_with_rx().await;
    assert!(
        !super::handlers::reject_workflow_managed_operation(
            ordinary_session.as_ref(),
            "ordinary-op".to_string(),
            operation,
        )
        .await
    );
}

#[tokio::test]
async fn workflow_child_rejects_user_input_without_internal_turn_admission() {
    let (session, _turn_context, events) = make_workflow_session_and_context_with_rx().await;

    super::handlers::user_input_or_turn_inner(
        &session,
        "direct-user-input".to_string(),
        Op::UserInput {
            items: vec![codex_protocol::user_input::UserInput::Text {
                text: "must not steer the workflow child".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides::default(),
        },
        /*client_user_message_id*/ None,
    )
    .await;

    let event = events.recv().await.expect("direct input rejection event");
    let EventMsg::Error(error) = event.msg else {
        panic!("expected direct input rejection");
    };
    assert_eq!(event.id, "direct-user-input");
    assert_eq!(
        (error.message.as_str(), error.codex_error_info),
        (
            "direct user input is not allowed for workflow-managed threads",
            Some(CodexErrorInfo::BadRequest),
        )
    );
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(session.clone_history().await.raw_items(), &[]);
}

#[tokio::test]
async fn finalized_generic_context_reserves_the_exact_post_metadata_user_prompt() {
    let (session, mut turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    let fixed_turn_id = "00000000-0000-7000-8000-000000000000";
    let mutable_turn_context =
        Arc::get_mut(&mut turn_context).expect("turn context should not be shared");
    mutable_turn_context.sub_id = fixed_turn_id.to_string();
    mutable_turn_context.history_mode = ThreadHistoryMode::Paginated;

    for index in 0..8 {
        session
            .record_conversation_items(
                turn_context.as_ref(),
                &[message(
                    "developer",
                    format!(
                        "context-{index}:{}",
                        "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES * 2)
                    ),
                )],
            )
            .await;
    }
    let before_prompt = session.clone_history().await.raw_items().to_vec();
    assert_eq!(before_prompt.len(), 7);
    assert!(before_prompt.iter().all(|item| {
        serde_json::to_vec(item)
            .expect("serialize finalized context")
            .len()
            == MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    }));
    assert_eq!(
        before_prompt
            .iter()
            .map(|item| serde_json::to_vec(item).expect("serialize context").len())
            .sum::<usize>(),
        MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES
    );

    let empty_final_prompt = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", fixed_turn_id)),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: String::new(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some(fixed_turn_id.to_string()),
        }),
    };
    let prompt_envelope_bytes = serde_json::to_vec(&empty_final_prompt)
        .expect("serialize empty finalized prompt")
        .len();
    let prompt_text = "p".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES - prompt_envelope_bytes);
    let input = vec![UserInput::Text {
        text: prompt_text.clone(),
        text_elements: Vec::new(),
    }];

    assert!(
        session
            .record_user_prompt_and_emit_turn_item(
                turn_context.as_ref(),
                input.as_slice(),
                /*client_id*/ None,
            )
            .await
    );

    let history = session.clone_history().await.raw_items().to_vec();
    assert_eq!(history.len(), 8);
    assert_eq!(
        crate::context::validate_workflow_child_model_history(&history),
        Ok(())
    );
    assert_eq!(
        history
            .iter()
            .map(|item| serde_json::to_vec(item)
                .expect("serialize finalized history")
                .len())
            .sum::<usize>(),
        crate::context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
    );
    let Some(ResponseItem::Message {
        id: Some(id),
        role,
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    }) = history.last()
    else {
        panic!("expected finalized user prompt")
    };
    assert!(id.as_str().starts_with("msg_"));
    assert_eq!(role, "user");
    assert_eq!(content, &[ContentItem::InputText { text: prompt_text }]);
    assert_eq!(metadata.turn_id.as_deref(), Some(fixed_turn_id));
    assert_eq!(
        serde_json::to_vec(history.last().expect("final prompt"))
            .expect("serialize final prompt")
            .len(),
        MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
}

#[tokio::test]
async fn rejected_finalized_prompt_stops_without_recording_hook_context() {
    let (session, turn_context, events) = make_workflow_session_and_context_with_rx().await;

    assert!(
        crate::hook_runtime::record_pending_input(
            &session,
            &turn_context,
            super::TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "x".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
            vec!["must not be recorded after prompt rejection".to_string()],
        )
        .await
    );

    assert_eq!(session.clone_history().await.raw_items(), &[]);
    let event = events.recv().await.expect("prompt rejection event");
    let EventMsg::Error(error) = event.msg else {
        panic!("expected prompt rejection error")
    };
    assert_eq!(
        (error.message.as_str(), error.codex_error_info),
        (
            "workflow child prompt exceeds finalized model-context limits",
            Some(CodexErrorInfo::BadRequest),
        )
    );
}

#[tokio::test]
async fn finalized_generic_append_never_rewrites_assistant_output() {
    let (session, mut turn_context, _events) = make_workflow_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should not be shared")
        .history_mode = ThreadHistoryMode::Paginated;
    let assistant = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "unchanged-assistant")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "model output".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let expected = session
        .prepare_conversation_items_for_history(
            turn_context.as_ref(),
            std::slice::from_ref(&assistant),
        )
        .into_owned();

    session
        .record_conversation_items(turn_context.as_ref(), &[assistant])
        .await;

    assert_eq!(
        session.clone_history().await.raw_items(),
        expected.as_slice()
    );
    assert!(
        serde_json::to_vec(&expected[0])
            .expect("serialize unchanged assistant output")
            .len()
            > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
}

#[tokio::test]
async fn ordinary_generic_append_retains_existing_behavior() {
    let (session, mut turn_context, _events) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn_context)
        .expect("turn context should not be shared")
        .history_mode = ThreadHistoryMode::Paginated;
    let injected = ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "unchanged-ordinary")),
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "\0é".repeat(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let expected = session
        .prepare_conversation_items_for_history(
            turn_context.as_ref(),
            std::slice::from_ref(&injected),
        )
        .into_owned();

    session
        .record_conversation_items(turn_context.as_ref(), &[injected])
        .await;

    assert_eq!(
        session.clone_history().await.raw_items(),
        expected.as_slice()
    );
    assert!(
        serde_json::to_vec(&expected[0])
            .expect("serialize unchanged ordinary context")
            .len()
            > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
    );
}
