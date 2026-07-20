use super::RunAgentSummary;
use super::RunWatchBudget;
use super::RunWatchNode;
use super::RunWatchNodeKind;
use super::RunWatchPhase;
use super::RunWatchUnprojectedAgent;
use super::RunWatchView;
use super::load_run_agent_links;
use super::read_bounded_run_meta;
use super::run_index_params;
use crate::config::Config;
use crate::tools::code_mode::workflow_progress::durable::DurableNode;
use crate::tools::code_mode::workflow_progress::durable::DurableNodeState;
use crate::tools::code_mode::workflow_progress::durable::DurablePhaseState;
use crate::tools::code_mode::workflow_progress::durable::DurableProgressRead;
use crate::tools::code_mode::workflow_progress::durable::DurableRunState;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_workflow_journal::WorkflowRunStatus as JournalRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use std::collections::HashMap;
use std::io;
use std::io::SeekFrom;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

const MAX_WATCH_AGENTS: usize = 1_000;
const MAX_ROLLOUT_SUMMARIES: usize = 64;
const MAX_ROLLOUT_SUMMARY_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ROLLOUT_SUMMARY_TEXT_CHARS: usize = 240;

pub(super) async fn inspect_run(config: &Config, run_id: &str) -> anyhow::Result<RunWatchView> {
    if !config.features.enabled(Feature::Workflow) {
        anyhow::bail!(
            "the `workflow` feature must be enabled to watch a workflow run (pass `--enable workflow`)"
        );
    }
    uuid::Uuid::parse_str(run_id)
        .map_err(|error| anyhow::anyhow!("invalid workflow run id `{run_id}`: {error}"))?;

    let recovery = crate::workflow_recovery::reconcile_stale_workflow_run(
        config.codex_home.as_path(),
        run_id,
        /*state_db*/ None,
    )
    .await;

    let paths = WorkflowRunPaths::new(config.codex_home.as_path(), run_id);
    let meta = read_bounded_run_meta(&paths).await?;
    if meta.run_id != run_id {
        anyhow::bail!(
            "workflow metadata run id `{}` does not match requested run `{run_id}`",
            meta.run_id
        );
    }

    let was_reconciled = recovery.reconciled > 0;
    let legacy_lifecycle_unknown = recovery
        .legacy_unknown_run_ids
        .iter()
        .any(|id| id == run_id);
    let mut warnings = recovery.diagnostics;
    if was_reconciled {
        warnings.push(
            "the workflow's former process exited; this run was marked interrupted".to_string(),
        );
        match codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.model_provider_id.clone(),
        )
        .await
        {
            Ok(runtime) => {
                match run_index_params(config.codex_home.as_path(), &meta).await {
                    Ok(params) => {
                        if let Err(error) = runtime.upsert_workflow_run(&params).await {
                            warnings.push(format!(
                                "workflow discovery projection update failed after recovery: {error}"
                            ));
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "workflow discovery projection is unavailable after recovery: {error}"
                    )),
                }
                runtime.close().await;
            }
            Err(error) => warnings.push(format!(
                "workflow discovery projection is unavailable after recovery: {error}"
            )),
        }
    }
    let progress = match crate::tools::code_mode::workflow_progress::durable::read(
        config.codex_home.as_path(),
        run_id,
    )
    .await?
    {
        DurableProgressRead::Missing => {
            warnings.push(
                "progress snapshot is missing; showing metadata and journal fallback".to_string(),
            );
            None
        }
        DurableProgressRead::Corrupt(error) => {
            warnings.push(format!(
                "progress snapshot is corrupt; showing metadata and journal fallback: {error}"
            ));
            None
        }
        DurableProgressRead::Snapshot(snapshot) => Some(snapshot),
    };
    let linked_agents = if paths.journal().try_exists()? {
        match load_run_agent_links(&paths, run_id, MAX_WATCH_AGENTS).await {
            Ok(agents) => agents,
            Err(error) => {
                warnings.push(format!("agent journal fallback is unavailable: {error}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let rollout_summaries = load_rollout_summaries(&linked_agents).await;
    let mut linked_by_thread = linked_agents
        .iter()
        .map(|agent| (agent.thread_id.to_string(), agent))
        .collect::<HashMap<_, _>>();

    let mut phases = Vec::new();
    let mut nodes = Vec::new();
    let mut budget = None;
    let mut progress_status = None;
    let mut progress_terminal = false;
    if let Some(snapshot) = progress {
        progress_status = Some(run_status_name(&snapshot.status).to_string());
        progress_terminal = snapshot.state == DurableRunState::Terminal;
        budget = snapshot.budget.map(|budget| RunWatchBudget {
            spent: budget.spent,
            total: budget.total,
        });
        phases.extend(snapshot.phases.values().map(|phase| {
            RunWatchPhase {
                index: phase.index,
                title: phase.title.clone(),
                status: match phase.state {
                    DurablePhaseState::Pending => "pending",
                    DurablePhaseState::Active => "active",
                    DurablePhaseState::Completed => "completed",
                }
                .to_string(),
                implicit: phase.implicit,
            }
        }));
        for node in snapshot.topology.values() {
            match node {
                DurableNode::Group(group) => nodes.push(RunWatchNode {
                    id: group.id,
                    parent_node_id: group.parent_node_id,
                    phase_index: group.phase_index,
                    kind: RunWatchNodeKind::Group {
                        kind: match group.kind {
                            codex_protocol::protocol::WorkflowGroupKind::Parallel => "parallel",
                            codex_protocol::protocol::WorkflowGroupKind::Pipeline => "pipeline",
                        }
                        .to_string(),
                        item_count: group.item_count,
                        status: node_state_name(group.state).to_string(),
                    },
                }),
                DurableNode::Agent(agent) => {
                    let child_thread_id = agent
                        .child_thread_id
                        .as_deref()
                        .and_then(|thread_id| ThreadId::from_string(thread_id).ok());
                    let rollout_summary = child_thread_id
                        .as_ref()
                        .and_then(|thread_id| rollout_summaries.get(&thread_id.to_string()))
                        .cloned();
                    if let Some(thread_id) = child_thread_id.as_ref() {
                        linked_by_thread.remove(&thread_id.to_string());
                    }
                    nodes.push(RunWatchNode {
                        id: agent.id,
                        parent_node_id: agent.parent_node_id,
                        phase_index: agent.phase_index,
                        kind: RunWatchNodeKind::Agent {
                            label: agent
                                .label
                                .clone()
                                .unwrap_or_else(|| format!("agent {}", agent.id)),
                            model: agent.model.clone(),
                            effort: agent
                                .effort
                                .as_ref()
                                .map(|effort| format!("{effort:?}").to_ascii_lowercase()),
                            child_thread_id,
                            status: agent_status_name(&agent.status).to_string(),
                            total_tokens: agent.token_usage.total_tokens,
                            tool_call_count: agent.tool_call_count,
                            returned_null: agent.returned_null,
                            rollout_summary,
                        },
                    });
                }
            }
        }
    }

    let mut unprojected_agents = linked_by_thread
        .into_values()
        .map(|agent| RunWatchUnprojectedAgent {
            ordinal: agent.ordinal,
            thread_id: agent.thread_id,
            rollout_summary: rollout_summaries.get(&agent.thread_id.to_string()).cloned(),
        })
        .collect::<Vec<_>>();
    unprojected_agents.sort_by_key(|agent| agent.ordinal);
    let meta_status = match meta.status {
        JournalRunStatus::Running => "running",
        JournalRunStatus::Completed => "completed",
        JournalRunStatus::Stopped => "stopped",
        JournalRunStatus::Paused => "paused",
        JournalRunStatus::Failed => "failed",
    };
    let meta_terminal = meta.status != JournalRunStatus::Running;
    let unresolved_legacy_lifecycle =
        legacy_lifecycle_unknown && !meta_terminal && !progress_terminal;
    if unresolved_legacy_lifecycle {
        warnings.push(
            "this run predates durable workflow lifecycle tracking; its final status is unknown, so watch is stopping after this snapshot"
                .to_string(),
        );
    }
    let terminal = if unresolved_legacy_lifecycle {
        true
    } else {
        meta_terminal || progress_terminal
    };
    let status = if unresolved_legacy_lifecycle {
        "unknown".to_string()
    } else if meta_terminal {
        meta_status.to_string()
    } else {
        progress_status.unwrap_or_else(|| meta_status.to_string())
    };

    Ok(RunWatchView {
        run_id: run_id.to_string(),
        name: meta.name,
        status,
        terminal,
        phases,
        nodes,
        budget,
        unprojected_agents,
        warnings,
    })
}

async fn load_rollout_summaries(agents: &[RunAgentSummary]) -> HashMap<String, String> {
    let count = agents.len().min(MAX_ROLLOUT_SUMMARIES);
    if count == 0 {
        return HashMap::new();
    }
    let per_rollout_cap = MAX_ROLLOUT_SUMMARY_TOTAL_BYTES / count as u64;
    let mut summaries = HashMap::new();
    for agent in agents.iter().rev().take(count) {
        if let Ok(Some(summary)) =
            read_rollout_tail_summary(&agent.rollout_path, per_rollout_cap).await
        {
            summaries.insert(agent.thread_id.to_string(), summary);
        }
    }
    summaries
}

async fn read_rollout_tail_summary(
    path: &std::path::Path,
    max_bytes: u64,
) -> io::Result<Option<String>> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "child rollout is not a regular, non-symlink file",
        ));
    }
    let read_bytes = metadata.len().min(max_bytes);
    let start = metadata.len().saturating_sub(read_bytes);
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut bytes = Vec::with_capacity(read_bytes as usize);
    (&mut file).take(read_bytes).read_to_end(&mut bytes).await?;
    if start > 0
        && let Some(newline) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=newline);
    }

    let mut summary = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(line) = serde_json::from_slice::<RolloutLine>(line) else {
            continue;
        };
        if let Some(text) = assistant_text(&line.item) {
            summary = Some(bound_rollout_summary(&text));
        }
    }
    Ok(summary)
}

fn assistant_text(item: &RolloutItem) -> Option<String> {
    match item {
        RolloutItem::EventMsg(EventMsg::AgentMessage(event)) => Some(event.message.clone()),
        RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
            if role == "assistant" =>
        {
            let text = content
                .iter()
                .filter_map(|item| match item {
                    ContentItem::OutputText { text } | ContentItem::InputText { text } => {
                        Some(text.as_str())
                    }
                    ContentItem::InputImage { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            (!text.trim().is_empty()).then_some(text)
        }
        RolloutItem::SessionMeta(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::EventMsg(_) => None,
    }
}

fn bound_rollout_summary(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let prefix = characters
        .by_ref()
        .take(MAX_ROLLOUT_SUMMARY_TEXT_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn agent_status_name(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::PendingInit => "pending",
        AgentStatus::Running => "running",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Errored(_) => "failed",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}

fn run_status_name(status: &DurableRunStatus) -> &'static str {
    match status {
        DurableRunStatus::PendingInit => "pending",
        DurableRunStatus::Running => "running",
        DurableRunStatus::Interrupted => "interrupted",
        DurableRunStatus::Completed(_) => "completed",
        DurableRunStatus::Errored(_) => "failed",
        // `shutdown` only appears in legacy progress snapshots. New explicit
        // user stops are reduced to `stopped` without changing protocol wire types.
        DurableRunStatus::Shutdown => "shutdown",
        DurableRunStatus::NotFound => "not_found",
        DurableRunStatus::Stopped => "stopped",
        DurableRunStatus::Paused => "paused",
    }
}

fn node_state_name(state: DurableNodeState) -> &'static str {
    match state {
        DurableNodeState::Active => "active",
        DurableNodeState::Completed => "completed",
    }
}
