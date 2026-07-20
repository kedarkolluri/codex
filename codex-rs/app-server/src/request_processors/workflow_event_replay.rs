//! Replays a bounded workflow progress snapshot when a client resumes a thread.
//!
//! Live workflow events are already persisted in the thread rollout. Replaying them directly
//! through core would persist duplicates and notify unrelated listeners, so resume projects a
//! connection-scoped stream instead. Counter updates are coalesced per agent and narration is
//! bounded; lifecycle events retain their original order so the TUI can rebuild topology.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::WorkflowEvent;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::workflow_event_mapping::workflow_event_to_server_notification;

const MAX_REPLAY_RUNS: usize = 4;
const MAX_LIFECYCLE_EVENTS_PER_RUN: usize = 20_000;
const MAX_LOG_EVENTS_PER_RUN: usize = 100;

#[derive(Default)]
struct RunReplay {
    begin: Option<(usize, WorkflowEvent)>,
    lifecycle: Vec<(usize, WorkflowEvent)>,
    agent_updates: HashMap<(u64, u32), (usize, WorkflowEvent)>,
    ended_agents: HashSet<(u64, u32)>,
    logs: VecDeque<(usize, WorkflowEvent)>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReplayRunCandidate {
    latest_sequence: usize,
    terminal: bool,
}

impl RunReplay {
    fn push(&mut self, sequence: usize, event: WorkflowEvent) {
        match &event {
            WorkflowEvent::RunBegin(_) => self.begin = Some((sequence, event)),
            WorkflowEvent::AgentUpdated(update) => {
                let key = (update.node_id, update.attempt);
                if !self.ended_agents.contains(&key) {
                    self.agent_updates.insert(key, (sequence, event));
                }
            }
            WorkflowEvent::AgentEnd(completed) => {
                let key = (completed.node_id, completed.attempt);
                self.ended_agents.insert(key);
                self.agent_updates.remove(&key);
                self.push_lifecycle(sequence, event);
            }
            WorkflowEvent::Log(_) => {
                if self.logs.len() == MAX_LOG_EVENTS_PER_RUN {
                    self.logs.pop_front();
                }
                self.logs.push_back((sequence, event));
            }
            WorkflowEvent::RunEnd(_)
            | WorkflowEvent::PhaseBegin(_)
            | WorkflowEvent::PhaseEnd(_)
            | WorkflowEvent::GroupBegin(_)
            | WorkflowEvent::GroupEnd(_)
            | WorkflowEvent::AgentBegin(_)
            | WorkflowEvent::AgentBound(_) => self.push_lifecycle(sequence, event),
        }
    }

    fn push_lifecycle(&mut self, sequence: usize, event: WorkflowEvent) {
        if self.lifecycle.len() < MAX_LIFECYCLE_EVENTS_PER_RUN {
            self.lifecycle.push((sequence, event));
        }
    }

    fn into_events(self) -> Vec<(usize, WorkflowEvent)> {
        let mut events = Vec::with_capacity(
            usize::from(self.begin.is_some())
                + self.lifecycle.len()
                + self.agent_updates.len()
                + self.logs.len(),
        );
        events.extend(self.begin);
        events.extend(self.lifecycle);
        events.extend(self.agent_updates.into_values());
        events.extend(self.logs);
        events.sort_by_key(|(sequence, _)| *sequence);
        events
    }
}

/// Sends the retained workflow history only to the connection that resumed the thread.
pub(super) async fn send_workflow_replay_to_connection(
    outgoing: &Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    thread_id: ThreadId,
    rollout_items: &[RolloutItem],
) {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    for event in workflow_events_for_resume(rollout_items) {
        outgoing
            .send_server_notification_to_connections(
                &[connection_id],
                workflow_event_to_server_notification(&thread_id.to_string(), event, observed_at),
            )
            .await;
    }
}

fn workflow_events_for_resume(rollout_items: &[RolloutItem]) -> Vec<WorkflowEvent> {
    let mut candidates = HashMap::<String, ReplayRunCandidate>::new();
    for (sequence, item) in rollout_items.iter().enumerate() {
        let RolloutItem::EventMsg(EventMsg::Workflow(event)) = item else {
            continue;
        };
        let candidate = candidates
            .entry(workflow_run_id(event).to_string())
            .or_default();
        candidate.latest_sequence = sequence;
        match event {
            WorkflowEvent::RunBegin(_) => candidate.terminal = false,
            WorkflowEvent::RunEnd(_) => candidate.terminal = true,
            WorkflowEvent::PhaseBegin(_)
            | WorkflowEvent::PhaseEnd(_)
            | WorkflowEvent::GroupBegin(_)
            | WorkflowEvent::GroupEnd(_)
            | WorkflowEvent::AgentBegin(_)
            | WorkflowEvent::AgentBound(_)
            | WorkflowEvent::AgentUpdated(_)
            | WorkflowEvent::AgentEnd(_)
            | WorkflowEvent::Log(_) => {}
        }
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by(|(left_run_id, left), (right_run_id, right)| {
        left.terminal
            .cmp(&right.terminal)
            .then_with(|| right.latest_sequence.cmp(&left.latest_sequence))
            .then_with(|| left_run_id.cmp(right_run_id))
    });
    let selected_run_ids = candidates
        .into_iter()
        .take(MAX_REPLAY_RUNS)
        .map(|(run_id, _)| run_id)
        .collect::<HashSet<_>>();
    let mut runs = selected_run_ids
        .iter()
        .cloned()
        .map(|run_id| (run_id, RunReplay::default()))
        .collect::<HashMap<_, _>>();

    for (sequence, item) in rollout_items.iter().enumerate() {
        let RolloutItem::EventMsg(EventMsg::Workflow(event)) = item else {
            continue;
        };
        let run_id = workflow_run_id(event).to_string();
        if let Some(run) = runs.get_mut(&run_id) {
            run.push(sequence, event.clone());
        }
    }

    let mut events = runs
        .into_values()
        .flat_map(RunReplay::into_events)
        .collect::<Vec<_>>();
    events.sort_by_key(|(sequence, _)| *sequence);
    events.into_iter().map(|(_, event)| event).collect()
}

fn workflow_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

#[cfg(test)]
#[path = "workflow_event_replay_tests.rs"]
mod tests;
