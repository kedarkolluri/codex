use codex_protocol::ThreadId;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::WorkflowSupervisorOwnership;

/// Selects the trusted owner of a spawned child's terminal result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ParentCompletionDelivery {
    /// Deliver the standard completion envelope to the direct parent agent.
    #[default]
    NotifyParent,
    /// Reserve terminal-result delivery for the workflow supervisor.
    WorkflowSupervisor {
        ownership: WorkflowSupervisorOwnership,
    },
}

impl ParentCompletionDelivery {
    pub(crate) fn notifies_parent(self) -> bool {
        matches!(self, Self::NotifyParent)
    }

    pub(crate) fn workflow_supervisor_ownership(self) -> Option<WorkflowSupervisorOwnership> {
        match self {
            Self::NotifyParent => None,
            Self::WorkflowSupervisor { ownership } => Some(ownership),
        }
    }
}

/// Resolves runtime delivery from trusted fresh-spawn intent or canonical resumed metadata.
pub(crate) fn resolve_parent_completion_delivery(
    thread_id: ThreadId,
    initial_history: &InitialHistory,
    requested: ParentCompletionDelivery,
) -> ParentCompletionDelivery {
    let InitialHistory::Resumed(resumed) = initial_history else {
        return requested;
    };
    let ownership = resumed.history.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == thread_id => {
            Some(meta_line.meta.workflow_supervisor_ownership)
        }
        _ => None,
    });
    match ownership.flatten() {
        Some(ownership) => ParentCompletionDelivery::WorkflowSupervisor { ownership },
        None => ParentCompletionDelivery::NotifyParent,
    }
}

#[cfg(test)]
#[path = "workflow_ownership_tests.rs"]
mod tests;
