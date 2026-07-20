use super::*;
use crate::thread_manager::GENERIC_COLLABORATION_SUBTREE_GATE_PERMITS;
use std::collections::HashSet;

impl AgentControl {
    /// Close an ordinary collaboration subtree without crossing into workflow-owned agents.
    ///
    /// Fresh spawns share a manager-wide gate with this operation. The live descendants are
    /// captured once, validated before any close mutation, and the same snapshot is passed to the
    /// shutdown path, so no newly registered child can be swept into an unvalidated shutdown.
    pub(crate) async fn close_agent_from_generic_collaboration(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<String> {
        let gate_state = self.upgrade()?;
        let _subtree_guard = gate_state
            .generic_collaboration_subtree_gate()
            .acquire_many(GENERIC_COLLABORATION_SUBTREE_GATE_PERMITS)
            .await
            .map_err(|_| {
                CodexErr::Fatal("generic collaboration subtree gate is closed".to_string())
            })?;
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let mut subtree_thread_ids = Vec::with_capacity(descendant_ids.len() + 1);
        subtree_thread_ids.push(agent_id);
        subtree_thread_ids.extend(descendant_ids.iter().copied());
        self.ensure_generic_collaboration_agents_allowed(&subtree_thread_ids)
            .await?;

        self.close_agent_with_captured_descendants(agent_id, descendant_ids)
            .await
    }

    /// Resume an ordinary collaboration subtree without crossing into workflow-owned agents.
    ///
    /// Fresh spawns share a manager-wide gate with this operation. Persisted open edges are
    /// captured and validated before the root thread is resumed, and the resume path consumes that
    /// exact snapshot.
    pub(crate) async fn resume_agent_from_rollout_from_generic_collaboration(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let gate_state = self.upgrade()?;
        let _subtree_guard = gate_state
            .generic_collaboration_subtree_gate()
            .acquire_many(GENERIC_COLLABORATION_SUBTREE_GATE_PERMITS)
            .await
            .map_err(|_| {
                CodexErr::Fatal("generic collaboration subtree gate is closed".to_string())
            })?;
        let (children_by_parent, subtree_thread_ids) =
            self.capture_open_persisted_agent_subtree(thread_id).await?;
        self.ensure_generic_collaboration_agents_allowed(&subtree_thread_ids)
            .await?;

        Box::pin(self.resume_agent_from_rollout_with_captured_descendants(
            config,
            thread_id,
            session_source,
            children_by_parent,
        ))
        .await
    }

    async fn capture_open_persisted_agent_subtree(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<(HashMap<ThreadId, Vec<ThreadId>>, Vec<ThreadId>)> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::new();
        let mut subtree_thread_ids = vec![root_thread_id];
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok((children_by_parent, subtree_thread_ids));
        };

        let mut seen_thread_ids = HashSet::from([root_thread_id]);
        let mut queue = VecDeque::from([root_thread_id]);
        while let Some(parent_thread_id) = queue.pop_front() {
            let child_ids = agent_graph_store
                .list_thread_spawn_children(
                    parent_thread_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to load persisted thread-spawn children for {parent_thread_id}: {err}"
                    ))
                })?;
            let child_ids = child_ids
                .into_iter()
                .filter(|child_thread_id| seen_thread_ids.insert(*child_thread_id))
                .collect::<Vec<_>>();
            for child_thread_id in &child_ids {
                subtree_thread_ids.push(*child_thread_id);
                queue.push_back(*child_thread_id);
            }
            if !child_ids.is_empty() {
                children_by_parent.insert(parent_thread_id, child_ids);
            }
        }

        Ok((children_by_parent, subtree_thread_ids))
    }

    /// Validate both the live source and the canonical persisted source for every candidate.
    /// Store failures are returned instead of treating an unreadable source as ordinary.
    async fn ensure_generic_collaboration_agents_allowed(
        &self,
        thread_ids: &[ThreadId],
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        for thread_id in thread_ids {
            let registry_marks_workflow = self
                .state
                .agent_metadata_for_thread(*thread_id)
                .is_some_and(|metadata| {
                    metadata.parent_completion_delivery
                        == ParentCompletionDelivery::WorkflowSupervisor
                });
            let live_thread_marks_workflow = state
                .get_thread(*thread_id)
                .await
                .is_ok_and(|thread| thread.is_workflow_managed_agent());
            if registry_marks_workflow || live_thread_marks_workflow {
                return Err(workflow_managed_collaboration_target_error());
            }

            match state
                .read_stored_thread(ReadThreadParams {
                    thread_id: *thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
            {
                Ok(stored_thread)
                    if is_workflow_managed_thread_source(stored_thread.thread_source.as_ref()) =>
                {
                    return Err(workflow_managed_collaboration_target_error());
                }
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

fn workflow_managed_collaboration_target_error() -> CodexErr {
    CodexErr::InvalidRequest(WORKFLOW_MANAGED_COLLABORATION_TARGET_ERROR.to_string())
}
