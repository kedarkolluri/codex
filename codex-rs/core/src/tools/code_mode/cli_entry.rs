//! `codex workflow run` entrypoint (`P4-cli-run`, spec §9 entrypoint 3).
//!
//! This is the non-interactive/CI launch path that finally makes the M0-M3
//! Dynamic Workflows ENGINE reachable. Everything downstream — parse `meta`, run
//! the body once in a fresh isolate with the workflow narrator globals installed
//! (`phase()`/`log()`/`agent()`/…), the journal, budget, resume — already exists
//! behind [`run_workflow_source_to_terminal`] / [`resume_workflow_source_to_terminal`]; before this ticket
//! there was simply no way to invoke it without going through the model.
//!
//! The critical property this path guarantees (and the reason the ticket exists)
//! is that the body runs with `workflow: true`, so a `phase()`/`log()`-only
//! workflow runs to completion with **zero model calls** — unlike the plain
//! code-mode `exec` tool, which runs the same source with `workflow: false` and
//! would throw `phase is not defined`.
//!
//! This entrypoint constructs the same registered [`Session`](crate::session::session::Session),
//! turn, tool router, and dispatch worker as production runtime commands. As a
//! result `agent()` fanout inherits the effective provider/auth/profile and
//! persists normal child rollouts instead of running against a detached isolate.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_extension_api::UserInstructionsProvider;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_protocol::protocol::SessionSource;
use codex_workflow_journal::JournalLine;
use codex_workflow_journal::storage::WorkflowRunPaths;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::init_state_db;
use crate::local_agent_graph_store_from_state_db;
use crate::resolve_installation_id;
use crate::session::turn::built_tools;
use crate::thread_manager::ThreadManager;
use crate::thread_manager::thread_store_from_config;
use crate::tools::context::SharedTurnDiffTracker;
use crate::turn_diff_tracker::TurnDiffTracker;

use super::ExecContext;
use super::workflow_handler::WorkflowRunLineage;
use super::workflow_handler::read_named_workflow_source_bounded;
use super::workflow_handler::read_workflow_source_bounded;
use super::workflow_handler::resume_workflow_source_to_terminal;
use super::workflow_handler::run_workflow_source_to_terminal;
use super::workflow_progress::WorkflowEventTarget;

const MAX_WORKFLOW_ARGS_BYTES: usize = 32 * 1024;
const MAX_NARRATION_FILE_BYTES: u64 = 512 * 1024;
const MAX_NARRATION_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_NARRATION_LINES: usize = 256;
const NARRATION_TRUNCATION_MARKER: &str = "… [truncated]";

/// Structured outcome of a `codex workflow run` invocation.
pub struct WorkflowCliOutput {
    /// Host-minted run id of this run (`workflow.runId`).
    pub run_id: String,
    /// The workflow's joined top-level `text(...)` return value, if any.
    pub output_text: Option<String>,
    /// A script error, if the body threw or the run failed at the isolate level.
    /// Present iff the run should be treated as a failure (non-zero exit).
    pub error_text: Option<String>,
    /// `phase()` / `log()` narration recovered from the run's `journal.jsonl`, in
    /// file order. Best-effort: empty when the run wrote no journal narration.
    pub narration: Vec<String>,
}

/// Resolve a `codex workflow run <target>` argument to workflow source text.
///
/// A raw script path (the primary case) is read directly. Otherwise the string is
/// treated as a saved workflow NAME and resolved through the same layered
/// core-workflows registry the nested `workflow(name)` path uses
/// (`<cwd>/.codex/workflows` > `$HOME/.agents/workflows` > `$CODEX_HOME/workflows`).
pub async fn resolve_workflow_target(config: &Config, target: &str) -> anyhow::Result<String> {
    let raw_path = Path::new(target);
    let path = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        config.cwd.join(raw_path).to_path_buf()
    };
    if tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return read_workflow_source_bounded(&path).await.map_err(|err| {
            anyhow::anyhow!("failed to read workflow script `{}`: {err}", path.display())
        });
    }

    // Not an existing path: resolve as a saved workflow name.
    let roots = codex_core_workflows::workflow_roots(
        Some(config.cwd.as_path()),
        dirs::home_dir().as_deref(),
        Some(config.codex_home.as_path()),
    );
    let registry = codex_core_workflows::load_workflows_from_roots(roots).await;
    match registry.resolve_by_name(target) {
        Some(metadata) => read_named_workflow_source_bounded(&metadata.path, target)
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "failed to read saved workflow `{target}` at {}: {err}",
                    metadata.path.display()
                )
            }),
        None => anyhow::bail!(
            "workflow `{target}` did not resolve to an existing script path or a saved workflow name"
        ),
    }
}

/// Run a workflow body once through the real engine (`workflow: true`) and return
/// its structured outcome.
///
/// Feature-gated behind [`Feature::Workflow`]; the caller is expected to have
/// enabled it (e.g. `codex --enable workflow workflow run …`). The code-mode
/// session provider is selected exactly as the production thread manager selects
/// it, so this shares the same isolate host path as an in-session workflow run.
pub async fn run_workflow_cli(
    config: &Config,
    source: &str,
    args: serde_json::Value,
    resume: Option<String>,
    user_instructions_provider: Arc<dyn UserInstructionsProvider>,
) -> anyhow::Result<WorkflowCliOutput> {
    let features = config.features.get();
    if !features.enabled(Feature::Workflow) {
        anyhow::bail!(
            "the `workflow` feature must be enabled to run a workflow (pass `--enable workflow`)"
        );
    }

    if let Some(source_run_id) = resume.as_deref() {
        let parsed = uuid::Uuid::parse_str(source_run_id)
            .map_err(|_| anyhow::anyhow!("invalid --resume run id `{source_run_id}`"))?;
        if parsed.to_string() != source_run_id {
            anyhow::bail!("invalid --resume run id `{source_run_id}`");
        }
    }

    let serialized_args = serde_json::to_vec(&args)
        .map_err(|err| anyhow::anyhow!("failed to serialize workflow args: {err}"))?;
    if serialized_args.len() > MAX_WORKFLOW_ARGS_BYTES {
        anyhow::bail!("workflow args exceed the {MAX_WORKFLOW_ARGS_BYTES}-byte execution cap");
    }

    // Construct the same registered Session/Turn graph used by interactive and
    // exec callers. This is essential for `agent()` because its spawn path needs
    // the parent thread in ThreadManager, the inherited effective provider/auth,
    // per-agent rollout persistence, and a live turn dispatch worker.
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ true).await;
    let runtime_paths = ExecServerRuntimePaths::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let environment_manager = Arc::new(
        EnvironmentManager::from_codex_home(config.codex_home.clone(), Some(runtime_paths)).await?,
    );
    let state_db = init_state_db(config).await;
    let thread_store = thread_store_from_config(config, state_db.clone());
    let thread_manager = ThreadManager::new(
        config,
        Arc::clone(&auth_manager),
        SessionSource::Exec,
        environment_manager,
        empty_extension_registry(),
        user_instructions_provider,
        /*analytics_events_client*/ None,
        thread_store,
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        resolve_installation_id(&config.codex_home).await?,
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let new_thread = thread_manager.start_thread(config.clone()).await?;

    let run_result: anyhow::Result<WorkflowCliOutput> = async {
        let session = &new_thread.thread.codex.session;
        let turn = session.new_default_turn().await;
        let step_context = session.capture_step_context(Arc::clone(&turn)).await;
        let cancellation_token = CancellationToken::new();
        let router = built_tools(session, step_context.as_ref(), &cancellation_token).await?;
        let tracker: SharedTurnDiffTracker =
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let _turn_worker = session
            .services
            .code_mode_service
            .start_turn_worker(
                session,
                Arc::clone(&step_context),
                Arc::clone(&router),
                tracker,
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("workflow runtime could not start its turn worker"))?;

        let enabled_tools =
            codex_tools::collect_code_mode_tool_definitions(&router.model_visible_specs());
        let call_id = format!("workflow-cli-{}", uuid::Uuid::now_v7());
        let service = &session.services.code_mode_service;
        let event_target = WorkflowEventTarget::session(ExecContext {
            session: Arc::clone(session),
            turn: Arc::clone(&turn),
        });
        let output = match resume.as_deref() {
            Some(source_run_id) => {
                resume_workflow_source_to_terminal(
                    features,
                    service,
                    call_id,
                    enabled_tools,
                    source,
                    args,
                    config.codex_home.as_path(),
                    source_run_id,
                    event_target,
                    cancellation_token.clone(),
                )
                .await
            }
            None => {
                run_workflow_source_to_terminal(
                    features,
                    service,
                    call_id,
                    enabled_tools,
                    source,
                    args,
                    WorkflowRunLineage {
                        parent_run_id: None,
                        depth: 0,
                    },
                    config.codex_home.as_path(),
                    None,
                    event_target,
                    cancellation_token.clone(),
                )
                .await
            }
        }
        .map_err(|err| anyhow::anyhow!("{err}"))?;

        let run_id = output.run_id;
        let (output_text, error_text) = split_response(output.response);
        let narration = read_narration(config.codex_home.as_path(), &run_id).await;
        Ok(WorkflowCliOutput {
            run_id,
            output_text,
            error_text,
            narration,
        })
    }
    .await;

    let shutdown = thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
    if !shutdown.submit_failed.is_empty() || !shutdown.timed_out.is_empty() {
        tracing::warn!(?shutdown, "workflow CLI did not cleanly stop every thread");
    }
    run_result
}

/// Split a terminal [`RuntimeResponse`] into `(joined text, script error)`.
///
/// The workflow's top-level result is the joined non-empty `text(...)` output; an
/// isolate-level `error_text` (a thrown body) is surfaced as the error so the CLI
/// exits non-zero. A `Yielded` response is not terminal on the CLI path (a
/// phase/log-only body runs to completion), and is reported as an error rather
/// than a silent success.
fn split_response(response: RuntimeResponse) -> (Option<String>, Option<String>) {
    let (content_items, error_text) = match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (content_items, error_text),
        RuntimeResponse::Terminated { content_items, .. } => (content_items, None),
        RuntimeResponse::Yielded { .. } => {
            return (
                None,
                Some("workflow did not run to completion (it yielded)".to_string()),
            );
        }
    };
    (join_text(&content_items), error_text)
}

/// Join the non-empty `text(...)` segments of a terminal response into one string.
fn join_text(content_items: &[FunctionCallOutputContentItem]) -> Option<String> {
    let segments = content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("\n"))
    }
}

/// Recover `phase()` / `log()` narration for a run from its `journal.jsonl`, in
/// file order. Best-effort: any read/parse failure yields an empty list rather
/// than failing the run (the journal is not authoritative for CLI output).
async fn read_narration(codex_home: &std::path::Path, run_id: &str) -> Vec<String> {
    let paths = WorkflowRunPaths::new(codex_home, run_id);
    let Ok(Ok(Some(bytes))) = tokio::task::spawn_blocking(move || {
        paths.read_journal_prefix_bounded(/* max_bytes */ MAX_NARRATION_FILE_BYTES)
    })
    .await
    else {
        return Vec::new();
    };
    let mut narration = Vec::new();
    let mut output_bytes = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        if narration.len() >= MAX_NARRATION_LINES || output_bytes >= MAX_NARRATION_OUTPUT_BYTES {
            break;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<JournalLine>(line) else {
            continue;
        };
        let mut rendered = match entry {
            JournalLine::Phase(phase) => format!("phase: {}", phase.title),
            JournalLine::Log(log) => format!("log: {}", log.message),
            JournalLine::AgentCall(_) | JournalLine::AgentBound(_) => continue,
        };
        let remaining = MAX_NARRATION_OUTPUT_BYTES
            .saturating_sub(output_bytes)
            .saturating_sub(1);
        if remaining == 0 {
            break;
        }
        if rendered.len() > remaining {
            if remaining < NARRATION_TRUNCATION_MARKER.len() {
                break;
            }
            let content_cap = remaining.saturating_sub(NARRATION_TRUNCATION_MARKER.len());
            let mut end = content_cap;
            while end > 0 && !rendered.is_char_boundary(end) {
                end -= 1;
            }
            rendered.truncate(end);
            rendered.push_str(NARRATION_TRUNCATION_MARKER);
        }
        output_bytes = output_bytes.saturating_add(rendered.len().saturating_add(1));
        narration.push(rendered);
    }
    narration
}

#[cfg(test)]
#[path = "cli_entry_tests.rs"]
mod tests;
