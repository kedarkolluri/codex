use super::*;

#[derive(Clone, Copy)]
enum AgentRegistration {
    Known,
    Unknown,
}

impl AgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread.codex.session.ensure_rollout_materialized().await;
            thread.codex.session.flush_rollout().await?;
            let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state.send_op(agent_id, Op::Shutdown {}).await
            };
            thread.wait_until_terminated().await;
            result
        } else {
            state.send_op(agent_id, Op::Shutdown {}).await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.forget_v2_residency(agent_id);
        self.state.release_spawned_thread(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    #[cfg(test)]
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let registration = self.mark_agent_closed(agent_id).await?;
        normalize_close_result(
            Box::pin(self.shutdown_agent_tree(agent_id)).await,
            registration,
        )
    }

    pub(super) async fn close_agent_with_captured_descendants(
        &self,
        agent_id: ThreadId,
        descendant_ids: Vec<ThreadId>,
    ) -> CodexResult<String> {
        let registration = self.mark_agent_closed(agent_id).await?;
        normalize_close_result(
            self.shutdown_captured_agent_tree(agent_id, descendant_ids)
                .await,
            registration,
        )
    }

    async fn mark_agent_closed(&self, agent_id: ThreadId) -> CodexResult<AgentRegistration> {
        let state = self.upgrade()?;
        let registration = if self.state.agent_metadata_for_thread(agent_id).is_some() {
            AgentRegistration::Known
        } else {
            AgentRegistration::Unknown
        };
        match state.get_thread(agent_id).await {
            Ok(thread) => {
                if !thread.config_snapshot().await.ephemeral
                    && let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
            }
            Err(CodexErr::ThreadNotFound(_))
                if matches!(registration, AgentRegistration::Known) =>
            {
                if let Some(agent_graph_store) = state.agent_graph_store()
                    && let Err(err) = agent_graph_store
                        .set_thread_spawn_edge_status(
                            agent_id,
                            codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
                        )
                        .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
            }
            Err(CodexErr::ThreadNotFound(_)) => {}
            Err(err) => {
                warn!("failed to inspect agent before close {agent_id}: {err}");
            }
        }
        Ok(registration)
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    #[cfg(test)]
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        self.shutdown_captured_agent_tree(agent_id, descendant_ids)
            .await
    }

    async fn shutdown_captured_agent_tree(
        &self,
        agent_id: ThreadId,
        descendant_ids: Vec<ThreadId>,
    ) -> CodexResult<String> {
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }
}

fn normalize_close_result(
    result: CodexResult<String>,
    registration: AgentRegistration,
) -> CodexResult<String> {
    match result {
        Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied)
            if matches!(registration, AgentRegistration::Known) =>
        {
            Ok(String::new())
        }
        result => result,
    }
}
