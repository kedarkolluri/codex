use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::bound_workflow_child_session_config;
use super::validate_resolved_workflow_child_base_instructions;
use super::tests::make_session_and_context_with_rx;
use super::tests::make_workflow_session_and_context_with_rx;
use crate::context::MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES;
use crate::context::MAX_WORKFLOW_CHILD_TOOL_OUTPUT_TOKENS;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::ThreadSource;

fn workflow_thread_source() -> ThreadSource {
    ThreadSource::Feature("workflow".to_string())
}

#[tokio::test]
async fn workflow_session_config_promotes_and_bounds_resumed_base_instructions() {
    let (_session, turn, _events) = make_session_and_context_with_rx().await;
    let mut config = (*turn.config).clone();
    config.base_instructions = None;
    config.tool_output_token_limit = Some(usize::MAX);
    let inherited = "resumed base".to_string();

    bound_workflow_child_session_config(
        &mut config,
        Some(inherited.clone()),
        Some(&workflow_thread_source()),
    )
    .expect("bounded resumed base should be accepted");

    assert_eq!(config.base_instructions, Some(inherited));
    assert_eq!(
        config.tool_output_token_limit,
        Some(MAX_WORKFLOW_CHILD_TOOL_OUTPUT_TOKENS)
    );
}

#[tokio::test]
async fn workflow_session_config_prefers_explicit_config_base_over_resumed_history() {
    let (_session, turn, _events) = make_session_and_context_with_rx().await;
    let mut config = (*turn.config).clone();
    config.base_instructions = Some("configured base".to_string());

    bound_workflow_child_session_config(
        &mut config,
        Some("x".repeat(8 * 1024 + 1)),
        Some(&workflow_thread_source()),
    )
    .expect("explicit bounded config base should take precedence");

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("configured base")
    );
}

#[tokio::test]
async fn workflow_session_config_rejects_oversized_resumed_history_base() {
    let (_session, turn, _events) = make_session_and_context_with_rx().await;
    let mut config = (*turn.config).clone();
    config.base_instructions = None;

    let error = bound_workflow_child_session_config(
        &mut config,
        Some("x".repeat(8 * 1024 + 1)),
        Some(&workflow_thread_source()),
    )
    .expect_err("oversized resumed base should be rejected");

    assert!(matches!(error, CodexErr::InvalidRequest(_)));
    assert!(!error.to_string().contains(&"x".repeat(128)));
}

#[tokio::test]
async fn workflow_session_config_rejects_oversized_resolved_model_base() {
    let (_session, turn, _events) = make_session_and_context_with_rx().await;
    let mut config = (*turn.config).clone();
    config.base_instructions = None;

    let error = validate_resolved_workflow_child_base_instructions(
        &config,
        &"x".repeat(8 * 1024 + 1),
        Some(&workflow_thread_source()),
    )
    .expect_err("oversized unaudited model base should be rejected");

    assert!(matches!(error, CodexErr::InvalidRequest(_)));
}

#[tokio::test]
async fn ordinary_session_config_remains_unchanged() {
    let (_session, turn, _events) = make_session_and_context_with_rx().await;
    let mut config = (*turn.config).clone();
    config.base_instructions = None;
    config.tool_output_token_limit = Some(usize::MAX);

    bound_workflow_child_session_config(
        &mut config,
        Some("x".repeat(8 * 1024 + 1)),
        Some(&ThreadSource::Subagent),
    )
    .expect("ordinary session config should not use workflow bounds");

    assert_eq!(config.base_instructions, None);
    assert_eq!(config.tool_output_token_limit, Some(usize::MAX));
}

#[tokio::test]
async fn workflow_record_boundary_bounds_tool_outputs_but_ordinary_sessions_are_unchanged() {
    let oversized_text = "x".repeat(MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES + 512);
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "recorded-output".to_string(),
        output: FunctionCallOutputPayload::from_text(oversized_text.clone()),
        internal_chat_message_metadata_passthrough: None,
    };

    let (workflow_session, workflow_turn, _workflow_events) =
        make_workflow_session_and_context_with_rx().await;
    workflow_session
        .record_conversation_items(workflow_turn.as_ref(), std::slice::from_ref(&output))
        .await;
    let workflow_history = workflow_session.clone_history().await;
    let [ResponseItem::FunctionCallOutput {
        output: workflow_output,
        ..
    }] = workflow_history.raw_items()
    else {
        panic!("expected one recorded workflow output");
    };
    assert!(
        serde_json::to_vec(workflow_output)
            .expect("workflow output should serialize")
            .len()
            <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES
    );

    let (ordinary_session, ordinary_turn, _ordinary_events) =
        make_session_and_context_with_rx().await;
    ordinary_session
        .record_conversation_items(ordinary_turn.as_ref(), std::slice::from_ref(&output))
        .await;
    let ordinary_history = ordinary_session.clone_history().await;
    let [ResponseItem::FunctionCallOutput {
        output: ordinary_output,
        ..
    }] = ordinary_history.raw_items()
    else {
        panic!("expected one recorded ordinary output");
    };
    assert_eq!(
        ordinary_output.body,
        FunctionCallOutputBody::Text(oversized_text)
    );
}
