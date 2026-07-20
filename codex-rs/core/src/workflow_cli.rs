//! Public entrypoints for the non-interactive Dynamic Workflows CLI.

use crate::config::Config;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_workflow_journal::RunAgentJournal;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus as JournalRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use codex_workflow_journal::storage::ensure_private_runs_root;
use tracing::warn;

const MAX_LIST_RUNS_LIMIT: usize = 1_000;
const MAX_REBUILD_RUNS: usize = 10_000;
const MAX_RUN_AGENTS_LIMIT: usize = 10_000;

#[path = "workflow_cli_watch.rs"]
mod watch;

pub use crate::tools::code_mode::cli_entry::WorkflowCliOutput as Output;
pub use crate::tools::code_mode::cli_entry::resolve_workflow_target as resolve_target;
pub use crate::tools::code_mode::cli_entry::run_workflow_cli as run;

/// A bounded-listing row from the rebuildable `workflow_runs` discovery index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: String,
    pub name: String,
    pub script_hash: String,
    pub script_path: String,
    pub parent_run_id: Option<String>,
    pub resumed_from_run_id: Option<String>,
    pub status: String,
    pub created_at: String,
}

/// One workflow-owned child transcript, reconstructed from the authoritative journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentSummary {
    pub ordinal: u64,
    pub thread_id: ThreadId,
    pub rollout_path: std::path::PathBuf,
}

/// One phase in the detached workflow-watch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWatchPhase {
    pub index: u64,
    pub title: String,
    pub status: String,
    pub implicit: bool,
}

/// One topology node in the detached workflow-watch projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWatchNode {
    pub id: u64,
    pub parent_node_id: Option<u64>,
    pub phase_index: u64,
    pub kind: RunWatchNodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunWatchNodeKind {
    Group {
        kind: String,
        item_count: u64,
        status: String,
    },
    Agent {
        label: String,
        model: Option<String>,
        effort: Option<String>,
        child_thread_id: Option<ThreadId>,
        status: String,
        total_tokens: i64,
        tool_call_count: u64,
        returned_null: bool,
        rollout_summary: Option<String>,
    },
}

/// A journal-linked child not present in a usable progress snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWatchUnprojectedAgent {
    pub ordinal: u64,
    pub thread_id: ThreadId,
    pub rollout_summary: Option<String>,
}

/// Terminal weighted-token usage and its optional workflow ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunWatchBudget {
    pub spent: i64,
    pub total: Option<i64>,
}

/// Bounded view used by `codex workflow watch <runId>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWatchView {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub terminal: bool,
    pub phases: Vec<RunWatchPhase>,
    pub nodes: Vec<RunWatchNode>,
    pub budget: Option<RunWatchBudget>,
    pub unprojected_agents: Vec<RunWatchUnprojectedAgent>,
    pub warnings: Vec<String>,
}

/// List workflow runs newest-first from the configured Codex home's discovery index.
///
/// The index is only a projection. If it is empty, rebuild it from bounded,
/// non-symlink `runs/<runId>/meta.json` files before listing.
pub async fn list_runs(config: &Config, limit: usize) -> anyhow::Result<Vec<RunSummary>> {
    if !config.features.enabled(Feature::Workflow) {
        anyhow::bail!(
            "the `workflow` feature must be enabled to list workflows (pass `--enable workflow`)"
        );
    }
    if !(1..=MAX_LIST_RUNS_LIMIT).contains(&limit) {
        anyhow::bail!("workflow run list limit must be from 1 to {MAX_LIST_RUNS_LIMIT}");
    }

    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    let recovery = crate::workflow_recovery::reconcile_stale_workflow_runs(
        config.codex_home.as_path(),
        Some(runtime.as_ref()),
    )
    .await;
    for diagnostic in recovery.diagnostics {
        warn!("workflow stale-run recovery: {diagnostic}");
    }
    let mut runs = runtime.list_workflow_runs(Some(limit)).await?;
    if runs.is_empty() {
        let recovered = recover_run_index(config.codex_home.as_path()).await?;
        if !recovered.is_empty() {
            runtime.rebuild_workflow_runs(&recovered).await?;
            runs = runtime.list_workflow_runs(Some(limit)).await?;
        }
    }
    runtime.close().await;
    Ok(runs
        .into_iter()
        .map(|run| RunSummary {
            run_id: run.run_id,
            name: run.name,
            script_hash: run.script_hash,
            script_path: run.script_path,
            parent_run_id: run.parent_run_id,
            resumed_from_run_id: run.resumed_from_run_id,
            status: run.status.as_str().to_string(),
            created_at: run.created_at,
        })
        .collect())
}

/// Rebuild and list one run's transcript grouping from its authoritative journal.
///
/// This refreshes the SQLite `run_agents` projection on every call, so a detached
/// monitor observes newly bound children without trusting stale index rows.
pub async fn list_run_agents(
    config: &Config,
    run_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<RunAgentSummary>> {
    if !config.features.enabled(Feature::Workflow) {
        anyhow::bail!(
            "the `workflow` feature must be enabled to inspect a workflow run (pass `--enable workflow`)"
        );
    }
    if !(1..=MAX_RUN_AGENTS_LIMIT).contains(&limit) {
        anyhow::bail!("workflow run agent limit must be from 1 to {MAX_RUN_AGENTS_LIMIT}");
    }
    uuid::Uuid::parse_str(run_id)
        .map_err(|error| anyhow::anyhow!("invalid workflow run id `{run_id}`: {error}"))?;

    let paths = WorkflowRunPaths::new(config.codex_home.as_path(), run_id);
    let linked_agents = load_run_agent_links(&paths, run_id, limit).await?;
    let mut recovered = Vec::with_capacity(linked_agents.len());
    for agent in &linked_agents {
        recovered.push(codex_state::WorkflowRunAgentUpsertParams {
            run_id: run_id.to_string(),
            ordinal: agent.ordinal,
            thread_id: agent.thread_id,
            rollout_path: agent.rollout_path.clone(),
        });
    }

    let durable_meta = read_bounded_run_meta(&paths).await?;
    if durable_meta.run_id != run_id {
        anyhow::bail!(
            "workflow metadata run id `{}` does not match requested run `{run_id}`",
            durable_meta.run_id
        );
    }
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.model_provider_id.clone(),
    )
    .await?;
    let result = async {
        if runtime.get_workflow_run(run_id).await?.is_none() {
            let params = run_index_params(config.codex_home.as_path(), &durable_meta).await?;
            runtime.upsert_workflow_run(&params).await?;
        }
        runtime
            .replace_workflow_run_agents(run_id, &recovered)
            .await?;
        runtime.list_workflow_run_agents(run_id, limit).await
    }
    .await;
    runtime.close().await;

    result.map(|agents| {
        agents
            .into_iter()
            .map(|agent| RunAgentSummary {
                ordinal: agent.ordinal,
                thread_id: agent.thread_id,
                rollout_path: agent.rollout_path,
            })
            .collect()
    })
}

async fn load_run_agent_links(
    paths: &WorkflowRunPaths,
    run_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<RunAgentSummary>> {
    let journal_path = paths.journal();
    let journal = tokio::task::spawn_blocking(move || RunAgentJournal::load(&journal_path))
        .await
        .map_err(|error| anyhow::anyhow!("workflow journal reader failed: {error}"))??;
    if journal.run_meta().run_id != run_id {
        anyhow::bail!(
            "workflow journal run id `{}` does not match requested run `{run_id}`",
            journal.run_meta().run_id
        );
    }
    if journal.links().len() > limit {
        anyhow::bail!(
            "workflow run `{run_id}` has more than the requested {limit}-agent inspection limit"
        );
    }
    journal
        .links()
        .iter()
        .map(|link| {
            let thread_id = ThreadId::from_string(&link.child_thread_id).map_err(|error| {
                anyhow::anyhow!(
                    "workflow run `{run_id}` ordinal {} has invalid child thread id: {error}",
                    link.ordinal
                )
            })?;
            Ok(RunAgentSummary {
                ordinal: link.ordinal,
                thread_id,
                rollout_path: link.rollout_path.clone(),
            })
        })
        .collect()
}

/// Read one bounded, cross-process workflow monitor frame.
///
/// `meta.json` and the journal remain authoritative fallbacks. A missing or corrupt progress
/// snapshot therefore yields a degraded frame instead of crashing the watcher, and terminal
/// metadata still stops polling deterministically if the final snapshot write failed.
pub async fn inspect_run(config: &Config, run_id: &str) -> anyhow::Result<RunWatchView> {
    watch::inspect_run(config, run_id).await
}

async fn recover_run_index(
    codex_home: &std::path::Path,
) -> anyhow::Result<Vec<codex_state::WorkflowRunUpsertParams>> {
    let root = ensure_private_runs_root(codex_home)?;
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut recovered = Vec::new();
    let mut inspected = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        inspected = inspected.saturating_add(1);
        if inspected > MAX_REBUILD_RUNS {
            anyhow::bail!(
                "workflow run index rebuild exceeds the {MAX_REBUILD_RUNS}-directory scan cap"
            );
        }
        let file_type = entry.file_type().await?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(directory_run_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let paths = WorkflowRunPaths::new(codex_home, &directory_run_id);
        let meta_path = paths.meta();
        let meta = match read_bounded_run_meta(&paths).await {
            Ok(meta) => meta,
            Err(error) => {
                warn!(
                    "failed to load workflow metadata {}: {error}",
                    meta_path.display()
                );
                continue;
            }
        };
        if meta.run_id != directory_run_id {
            warn!(
                "ignoring workflow metadata whose run id `{}` does not match directory `{directory_run_id}`",
                meta.run_id
            );
            continue;
        }
        match run_index_params(codex_home, &meta).await {
            Ok(params) => recovered.push(params),
            Err(error) => warn!(
                "failed to classify workflow lifecycle from {}: {error}",
                meta_path.display()
            ),
        }
    }
    Ok(recovered)
}

async fn read_bounded_run_meta(paths: &WorkflowRunPaths) -> anyhow::Result<WorkflowRunMeta> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || paths.read_meta_bounded())
        .await
        .map_err(|error| anyhow::anyhow!("workflow metadata reader failed: {error}"))?
        .map_err(Into::into)
}

async fn run_index_params(
    codex_home: &std::path::Path,
    meta: &WorkflowRunMeta,
) -> anyhow::Result<codex_state::WorkflowRunUpsertParams> {
    let status = match meta.status {
        JournalRunStatus::Running => {
            let paths = WorkflowRunPaths::new(codex_home, &meta.run_id);
            let lease_state =
                tokio::task::spawn_blocking(move || WorkflowRunLease::try_acquire_existing(&paths))
                    .await
                    .map_err(|error| anyhow::anyhow!("workflow lease reader failed: {error}"))??;
            match lease_state {
                Some(WorkflowRunLeaseAcquire::Acquired(lease)) => {
                    drop(lease);
                    // An unlocked lease-era run with Running metadata should have
                    // been terminalized by recovery. If it falls outside that
                    // bounded scan or changes between recovery and rebuild, its
                    // lifecycle is no longer safe to claim as running.
                    codex_state::WorkflowRunStatus::Unknown
                }
                Some(WorkflowRunLeaseAcquire::Held) => codex_state::WorkflowRunStatus::Running,
                None => codex_state::WorkflowRunStatus::Unknown,
            }
        }
        JournalRunStatus::Completed => codex_state::WorkflowRunStatus::Completed,
        JournalRunStatus::Stopped => codex_state::WorkflowRunStatus::Stopped,
        JournalRunStatus::Paused => codex_state::WorkflowRunStatus::Paused,
        JournalRunStatus::Failed => codex_state::WorkflowRunStatus::Failed,
    };
    Ok(codex_state::WorkflowRunUpsertParams {
        run_id: meta.run_id.clone(),
        name: meta.name.clone(),
        script_hash: meta.script_hash.clone(),
        script_path: WorkflowRunPaths::new(codex_home, &meta.run_id)
            .script()
            .display()
            .to_string(),
        parent_run_id: meta.parent_run_id.clone(),
        resumed_from_run_id: meta.resumed_from_run_id.clone(),
        owner_thread_id: meta.owner_thread_id.clone(),
        status,
        created_at: meta.created_at.clone(),
    })
}

#[cfg(test)]
#[path = "workflow_cli_tests.rs"]
mod tests;
