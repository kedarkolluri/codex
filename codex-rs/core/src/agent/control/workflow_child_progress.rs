//! Workflow-specific child identity, live counters, and durable terminal facts.

use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenUsage;
use codex_workflow_journal::AgentBoundLine;
use codex_workflow_journal::JournalRecorder;
use tracing::warn;

use super::AgentControl;
use super::spawn_await_opts::SpawnAgentConfigOverrides;
use crate::agent::role_context_bounds;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents::build_agent_spawn_config;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkflowChildProgress {
    pub(crate) token_usage: TokenUsage,
    pub(crate) tool_call_count: u64,
}

/// Ordered child lifecycle information delivered to a workflow host.
///
/// `Bound` is sent exactly once after the child is registered and before its first turn starts;
/// subsequent `Progress` values carry monotonic counter snapshots for that child.
pub(crate) enum WorkflowChildEvent {
    Bound {
        child_thread_id: ThreadId,
        acknowledged: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Progress(WorkflowChildProgress),
}

#[derive(Clone)]
struct WorkflowChildBindingJournal {
    recorder: Arc<JournalRecorder>,
    ordinal: u64,
    attempt: u32,
}

/// Event sink plus the optional durable binding context for one workflow child.
#[derive(Clone)]
pub(crate) struct WorkflowChildObserver {
    event_tx: tokio::sync::mpsc::UnboundedSender<WorkflowChildEvent>,
    binding_journal: Option<WorkflowChildBindingJournal>,
}

impl WorkflowChildObserver {
    pub(crate) fn new(event_tx: tokio::sync::mpsc::UnboundedSender<WorkflowChildEvent>) -> Self {
        Self {
            event_tx,
            binding_journal: None,
        }
    }

    pub(crate) fn with_binding_journal(
        mut self,
        recorder: Arc<JournalRecorder>,
        ordinal: u64,
        attempt: u32,
    ) -> Self {
        self.binding_journal = Some(WorkflowChildBindingJournal {
            recorder,
            ordinal,
            attempt,
        });
        self
    }

    /// Persist the run-to-child link before announcing it to the host. The caller invokes this
    /// after deferred registration and before submitting the child's first turn.
    pub(crate) async fn child_bound(
        &self,
        control: &AgentControl,
        child_thread_id: ThreadId,
    ) -> Result<(), String> {
        if let Some(binding_journal) = &self.binding_journal {
            let rollout_path = materialize_child_rollout_path(control, child_thread_id).await?;
            binding_journal
                .recorder
                .record_agent_bound(AgentBoundLine {
                    timestamp: None,
                    ordinal: binding_journal.ordinal,
                    attempt: binding_journal.attempt,
                    child_thread_id: child_thread_id.to_string(),
                    rollout_path: rollout_path.display().to_string(),
                })
                .await
                .map_err(|error| {
                    format!(
                        "failed to journal workflow child binding (ordinal {}): {error}",
                        binding_journal.ordinal
                    )
                })?;
        }
        let (acknowledged, acknowledgment) = tokio::sync::oneshot::channel();
        self.event_tx
            .send(WorkflowChildEvent::Bound {
                child_thread_id,
                acknowledged,
            })
            .map_err(|_| "workflow child binding observer closed".to_string())?;
        acknowledgment
            .await
            .map_err(|_| "workflow child binding acknowledgment dropped".to_string())?
    }

    pub(crate) fn progress(&self, progress: WorkflowChildProgress) {
        let _ = self.event_tx.send(WorkflowChildEvent::Progress(progress));
    }
}

#[derive(Debug, Default)]
pub(crate) struct ChildJournalFacts {
    pub(crate) rollout_path: Option<PathBuf>,
    pub(crate) token_usage: TokenUsage,
    pub(crate) tool_call_count: u64,
}

pub(crate) async fn prepare_spawn_config(
    control: &AgentControl,
    base_instructions: &BaseInstructions,
    parent_turn: &TurnContext,
    parent_thread_id: ThreadId,
    overrides: &SpawnAgentConfigOverrides,
) -> Option<crate::config::Config> {
    let mut config = match build_agent_spawn_config(base_instructions, parent_turn) {
        Ok(config) => config,
        Err(err) => {
            warn!("failed to build subagent spawn config: {err}");
            return None;
        }
    };
    if !overrides.is_empty() {
        let state = match control.upgrade() {
            Ok(state) => state,
            Err(err) => {
                warn!("thread manager dropped before resolving subagent overrides: {err}");
                return None;
            }
        };
        let parent_thread = match state.get_thread(parent_thread_id).await {
            Ok(parent_thread) => parent_thread,
            Err(err) => {
                warn!(
                    "parent thread {parent_thread_id} not registered while resolving subagent overrides: {err}"
                );
                return None;
            }
        };
        if let Err(err) = overrides
            .apply(&parent_thread.codex.session, parent_turn, &mut config)
            .await
        {
            warn!("failed to apply subagent model/effort overrides: {err}");
            return None;
        }
    } else if let Err(err) = overrides.apply_role_layer(&mut config).await {
        warn!("failed to apply default subagent role: {err}");
        return None;
    }
    if let Err(err) = role_context_bounds::bound_workflow_child_context(&mut config) {
        warn!("failed to bound workflow child context: {err}");
        return None;
    }
    Some(config)
}

pub(crate) fn observe_event(progress: &mut WorkflowChildProgress, event: &EventMsg) -> bool {
    match event {
        EventMsg::TokenCount(event) => {
            let Some(info) = &event.info else {
                return false;
            };
            if progress.token_usage == info.total_token_usage {
                return false;
            }
            progress.token_usage.clone_from(&info.total_token_usage);
            true
        }
        EventMsg::RawResponseItem(event) if is_tool_request(&event.item) => {
            progress.tool_call_count = progress.tool_call_count.saturating_add(1);
            true
        }
        _ => false,
    }
}

pub(crate) async fn child_journal_facts(
    control: &AgentControl,
    child_thread_id: ThreadId,
) -> ChildJournalFacts {
    let Ok(state) = control.upgrade() else {
        return ChildJournalFacts::default();
    };
    let Ok(child_thread) = state.get_thread(child_thread_id).await else {
        return ChildJournalFacts::default();
    };
    child_thread.ensure_rollout_materialized().await;
    let rollout_path = child_thread.rollout_path();
    let token_usage = child_thread
        .token_usage_info()
        .await
        .map(|info| info.total_token_usage)
        .unwrap_or_default();
    let tool_call_count = child_thread
        .codex
        .session
        .clone_history()
        .await
        .raw_items()
        .iter()
        .filter(|item| is_tool_request(item))
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    ChildJournalFacts {
        rollout_path,
        token_usage,
        tool_call_count,
    }
}

async fn materialize_child_rollout_path(
    control: &AgentControl,
    child_thread_id: ThreadId,
) -> Result<PathBuf, String> {
    let state = match control.upgrade() {
        Ok(state) => state,
        Err(error) => {
            return Err(format!(
                "thread manager dropped before journaling workflow child binding: {error}"
            ));
        }
    };
    let child_thread = match state.get_thread(child_thread_id).await {
        Ok(child_thread) => child_thread,
        Err(error) => {
            return Err(format!(
                "workflow child {child_thread_id} is not registered at binding: {error}"
            ));
        }
    };
    child_thread.ensure_rollout_materialized().await;
    child_thread.rollout_path().ok_or_else(|| {
        format!("workflow child {child_thread_id} has no materialized rollout path at binding")
    })
}

pub(crate) async fn replay_progress(rollout_path: Option<&str>) -> WorkflowChildProgress {
    let Some(rollout_path) = rollout_path else {
        return WorkflowChildProgress::default();
    };
    let items = match codex_rollout::RolloutRecorder::load_rollout_items(std::path::Path::new(
        rollout_path,
    ))
    .await
    {
        Ok((items, _, _)) => items,
        Err(error) => {
            warn!("failed to reconstruct workflow replay counters from {rollout_path}: {error}");
            return WorkflowChildProgress::default();
        }
    };
    let mut progress = WorkflowChildProgress::default();
    for item in items {
        match item {
            RolloutItem::ResponseItem(item) if is_tool_request(&item) => {
                progress.tool_call_count = progress.tool_call_count.saturating_add(1);
            }
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                if let Some(info) = event.info {
                    progress.token_usage = info.total_token_usage;
                }
            }
            _ => {}
        }
    }
    progress
}

fn is_tool_request(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
    )
}

#[cfg(test)]
#[path = "workflow_child_progress_tests.rs"]
mod tests;
