//! Bounded workflow progress bookkeeping owned by the isolate runtime.

pub(super) use codex_code_mode_protocol::WORKFLOW_LOG_MAX_EVENTS as MAX_LOG_EVENTS;
pub(super) use codex_code_mode_protocol::WORKFLOW_PHASE_MAX_EVENTS as MAX_PHASES;
pub(super) use codex_code_mode_protocol::WORKFLOW_TOPOLOGY_MAX_NODES as MAX_TOPOLOGY_NODES;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;

use super::RuntimeEvent;
use super::RuntimeState;

/// Reconnect retains at most two lifecycle events per topology node (begin/end), two per phase,
/// and the run endpoints. Agent updates are coalesced per active node and logs are retained under a
/// separate bound, keeping the reconstructable snapshot below the 20,000-event reconnect ceiling.
impl RuntimeState {
    pub(super) fn allocate_workflow_node_id(&mut self) -> Result<u64, &'static str> {
        if self.next_workflow_node_id >= MAX_TOPOLOGY_NODES {
            return Err("workflow topology node cap exceeded (maximum 4000)");
        }
        let node_id = self.next_workflow_node_id;
        self.next_workflow_node_id += 1;
        Ok(node_id)
    }

    pub(super) fn admit_workflow_log(&mut self) -> Result<(), &'static str> {
        if self.workflow_log_count >= MAX_LOG_EVENTS {
            return Err("workflow log event cap exceeded (maximum 4000)");
        }
        self.workflow_log_count += 1;
        Ok(())
    }

    pub(super) fn admit_workflow_phase(&mut self) -> Result<(), &'static str> {
        if self.workflow_phase_count >= MAX_PHASES {
            return Err("workflow phase cap exceeded (maximum 1000)");
        }
        self.workflow_phase_count += 1;
        Ok(())
    }

    pub(super) fn admit_workflow_output(
        &mut self,
        items: &[codex_code_mode_protocol::FunctionCallOutputContentItem],
    ) -> Result<(), String> {
        self.workflow_output_bounds.admit(items)
    }

    pub(super) fn emit_workflow_progress(&self, event: WorkflowEvent) {
        let _ = self
            .event_tx
            .send(RuntimeEvent::WorkflowProgress(Box::new(event)));
    }

    pub(super) fn finish_active_workflow_phase(&mut self) {
        if !self.active_workflow_nodes.is_empty() {
            return;
        }
        let Some((phase_index, title)) = self.active_workflow_phase.take() else {
            return;
        };
        let Some(run_id) = self.run_id.clone() else {
            return;
        };
        self.emit_workflow_progress(WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id,
            phase_index,
            title,
        }));
    }
}

pub(super) fn has_active_workflow_groups(scope: &mut v8::PinScope<'_, '_>) -> bool {
    scope
        .get_slot::<RuntimeState>()
        .is_some_and(|state| !state.active_workflow_groups.is_empty())
}
