use crate::agent::control::WORKFLOW_MANAGED_COLLABORATION_TARGET_ERROR;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::ThreadId;
use std::sync::Arc;

pub(crate) async fn ensure_collaboration_sender_allowed(
    session: &Session,
) -> Result<(), FunctionCallError> {
    if session.is_workflow_managed_agent().await {
        return Err(FunctionCallError::RespondToModel(
            "workflow-managed agents cannot use generic collaboration tools".to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn ensure_collaboration_target_allowed(
    session: &Session,
    thread_id: ThreadId,
) -> Result<(), FunctionCallError> {
    if session
        .services
        .agent_control
        .is_workflow_managed_agent(thread_id)
        .await
    {
        return Err(FunctionCallError::RespondToModel(
            WORKFLOW_MANAGED_COLLABORATION_TARGET_ERROR.to_string(),
        ));
    }
    Ok(())
}

/// Resolves a single tool-facing agent target to a thread id.
pub(crate) async fn resolve_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    register_session_root(session, turn);
    let thread_id = match ThreadId::from_string(target) {
        Ok(thread_id) => thread_id,
        Err(_) => session
            .services
            .agent_control
            .resolve_agent_reference(session.thread_id, &turn.session_source, target)
            .await
            .map_err(|err| match err {
                codex_protocol::error::CodexErr::UnsupportedOperation(message) => {
                    FunctionCallError::RespondToModel(message)
                }
                other => FunctionCallError::RespondToModel(other.to_string()),
            })?,
    };
    ensure_collaboration_target_allowed(session.as_ref(), thread_id).await?;
    Ok(thread_id)
}

fn register_session_root(session: &Arc<Session>, turn: &Arc<TurnContext>) {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);
}
