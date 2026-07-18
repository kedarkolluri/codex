use std::sync::Arc;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::AgentSpawnFuture;
use codex_code_mode_protocol::AgentSpawnOutcome;
use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateResponse;
use codex_code_mode_protocol::host::SessionId;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::peer::HostPeer;

pub(super) struct RemoteDelegate {
    session_id: SessionId,
    peer: Arc<HostPeer>,
}

impl RemoteDelegate {
    pub(super) fn new(session_id: SessionId, peer: Arc<HostPeer>) -> Self {
        Self { session_id, peer }
    }
}

impl CodeModeSessionDelegate for RemoteDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            match self
                .peer
                .call(
                    self.session_id.clone(),
                    DelegateRequest::InvokeTool {
                        invocation: invocation.into(),
                    },
                    cancellation_token,
                )
                .await?
            {
                DelegateResponse::ToolResult { result } => Ok(result),
                DelegateResponse::NotificationDelivered
                | DelegateResponse::AgentSpawned { .. }
                | DelegateResponse::WorkflowSpawned { .. } => {
                    Err("code-mode client returned an invalid tool result".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            match self
                .peer
                .call(
                    self.session_id.clone(),
                    DelegateRequest::Notify {
                        call_id,
                        cell_id: cell_id.into(),
                        text,
                    },
                    cancellation_token,
                )
                .await?
            {
                DelegateResponse::NotificationDelivered => Ok(()),
                DelegateResponse::ToolResult { .. }
                | DelegateResponse::AgentSpawned { .. }
                | DelegateResponse::WorkflowSpawned { .. } => {
                    Err("code-mode client returned an invalid notification result".to_string())
                }
            }
        })
    }

    /// Route a workflow `agent(prompt, opts?)` spawn over the wire to the client-side delegate (the
    /// real core spawn broker) and await the three-way outcome. Any wire failure, cancellation, or
    /// an unexpected response variant is death-is-null (`Failed`) — `agent()` never throws for a
    /// transport failure — while an explicit `Rejected` from the client is faithfully preserved so
    /// the isolate throws the cap/budget message.
    fn spawn_agent<'a>(
        &'a self,
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            match self
                .peer
                .call(
                    self.session_id.clone(),
                    DelegateRequest::SpawnAgent {
                        cell_id: cell_id.into(),
                        prompt,
                        ordinal,
                        opts: Box::new(opts),
                    },
                    cancellation_token,
                )
                .await
            {
                Ok(DelegateResponse::AgentSpawned { outcome }) => outcome.into(),
                Ok(_) | Err(_) => AgentSpawnOutcome::Failed,
            }
        })
    }

    /// Route a workflow `workflow(nameOrRef, args)` nested run over the wire to the client-side
    /// delegate (the real core workflow handler) and await the three-way outcome. Mirrors
    /// [`Self::spawn_agent`] exactly: any wire failure, cancellation, or an unexpected response
    /// variant is death-is-null (`Failed`), while an explicit `Rejected` from the client (e.g. an
    /// unresolvable name) is faithfully preserved so the isolate throws.
    fn spawn_workflow<'a>(
        &'a self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            match self
                .peer
                .call(
                    self.session_id.clone(),
                    DelegateRequest::SpawnWorkflow {
                        cell_id: cell_id.into(),
                        name,
                        args,
                    },
                    cancellation_token,
                )
                .await
            {
                Ok(DelegateResponse::WorkflowSpawned { outcome }) => outcome.into(),
                Ok(_) | Err(_) => AgentSpawnOutcome::Failed,
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.peer
            .close_cell(self.session_id.clone(), cell_id.clone());
    }
}
