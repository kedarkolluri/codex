use std::sync::Arc;

use codex_protocol::error::CodexErr;
use codex_protocol::AgentPath;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use pretty_assertions::assert_eq;

use super::Session;
use super::tests::make_session_and_context_with_rx;
use super::tests::make_workflow_session_and_context_with_rx;
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
