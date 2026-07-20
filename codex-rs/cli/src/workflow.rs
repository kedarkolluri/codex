//! Non-interactive Dynamic Workflows CLI commands.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::workflow_cli::RunSummary;
use codex_core::workflow_cli::RunWatchBudget;
use codex_core::workflow_cli::RunWatchNode;
use codex_core::workflow_cli::RunWatchNodeKind;
use codex_core::workflow_cli::RunWatchView;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_protocol::protocol::AskForApproval;
use codex_tui::Cli as TuiCli;
use codex_utils_cli::CliConfigOverrides;

use crate::loader_overrides_for_profile;

const MAX_WORKFLOW_ARGS_BYTES: usize = 32 * 1024;
const DEFAULT_WORKFLOW_LIST_LIMIT: usize = 50;
const MAX_WORKFLOW_LIST_LIMIT: usize = 1_000;
const WORKFLOW_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_WATCH_TREE_DEPTH: usize = 64;

#[derive(Debug, Parser)]
pub(crate) struct WorkflowCommand {
    #[command(subcommand)]
    action: WorkflowAction,
}

#[derive(Debug, clap::Subcommand)]
enum WorkflowAction {
    /// Run a workflow non-interactively by script path or saved name.
    Run(WorkflowRunArgs),

    /// List prior workflow runs newest-first.
    Ls(WorkflowListArgs),

    /// Watch a workflow run's durable live tree until it reaches a terminal state.
    Watch(WorkflowWatchArgs),
}

#[derive(Debug, Parser)]
struct WorkflowRunArgs {
    /// Workflow script path (.js) or the name of a saved workflow.
    #[arg(value_name = "NAME_OR_PATH")]
    target: String,

    /// JSON value injected read-only as the workflow `args` global.
    #[arg(long = "args", value_name = "JSON")]
    args: Option<String>,

    /// Resume a prior run by its runId (prefix-replay resume).
    #[arg(long = "resume", value_name = "RUN_ID")]
    resume: Option<String>,
}

#[derive(Debug, Parser)]
struct WorkflowListArgs {
    /// Maximum number of runs to print.
    #[arg(long, default_value_t = DEFAULT_WORKFLOW_LIST_LIMIT, value_parser = parse_list_limit)]
    limit: usize,
}

#[derive(Debug, Parser)]
struct WorkflowWatchArgs {
    /// Workflow run ID returned by a background start or prior invocation.
    #[arg(value_name = "RUN_ID")]
    run_id: String,

    /// Stream changed monitor frames as newline-delimited JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(
    cmd: WorkflowCommand,
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
    strict_config: bool,
) -> anyhow::Result<()> {
    match cmd.action {
        WorkflowAction::Run(run_args) => {
            if interactive.shared.oss {
                anyhow::bail!(
                    "`codex workflow run --oss` is not wired yet; configure an OpenAI-compatible router with `model_provider` or `-c model_provider=...`"
                );
            }
            let config = load_config(
                root_config_overrides,
                interactive,
                arg0_paths,
                strict_config,
            )
            .await?;
            run_workflow(run_args, &config).await
        }
        WorkflowAction::Ls(list_args) => {
            let config = load_config(
                root_config_overrides,
                interactive,
                arg0_paths,
                strict_config,
            )
            .await?;
            list_workflows(list_args, &config).await
        }
        WorkflowAction::Watch(watch_args) => {
            let config = load_config(
                root_config_overrides,
                interactive,
                arg0_paths,
                strict_config,
            )
            .await?;
            watch_workflow(watch_args, &config).await
        }
    }
}

async fn load_config(
    root_config_overrides: CliConfigOverrides,
    interactive: TuiCli,
    arg0_paths: Arg0DispatchPaths,
    strict_config: bool,
) -> anyhow::Result<Config> {
    let loader_overrides = loader_overrides_for_profile(interactive.config_profile_v2.as_ref())?;
    let shared = interactive.shared.into_inner();
    let mut cli_kv_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    if interactive.web_search {
        cli_kv_overrides.push((
            "web_search".to_string(),
            toml::Value::String("live".to_string()),
        ));
    }
    let sandbox_mode = if shared.dangerously_bypass_approvals_and_sandbox {
        Some(codex_protocol::config_types::SandboxMode::DangerFullAccess)
    } else {
        shared.sandbox_mode.map(Into::into)
    };
    let overrides = ConfigOverrides {
        model: shared.model,
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode,
        cwd: shared.cwd,
        codex_self_exe: arg0_paths.codex_self_exe,
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe,
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe,
        bypass_hook_trust: shared.bypass_hook_trust.then_some(true),
        additional_writable_roots: shared.add_dir,
        ..Default::default()
    };
    let config = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(overrides)
        .loader_overrides(loader_overrides)
        .strict_config(strict_config)
        .build()
        .await?;
    Ok(config)
}

/// Resolve and run a workflow directly through the workflow engine. A body that
/// uses only narration globals makes no model call; `agent()` uses the registered
/// production Session/Turn assembled by `codex_core::workflow_cli`.
async fn run_workflow(run_args: WorkflowRunArgs, config: &Config) -> anyhow::Result<()> {
    let WorkflowRunArgs {
        target,
        args,
        resume,
    } = run_args;

    let args_value = match args {
        Some(raw) => {
            if raw.len() > MAX_WORKFLOW_ARGS_BYTES {
                anyhow::bail!(
                    "--args JSON exceeds the {MAX_WORKFLOW_ARGS_BYTES}-byte execution cap"
                );
            }
            serde_json::from_str(&raw)
                .map_err(|err| anyhow::anyhow!("invalid --args JSON: {err}"))?
        }
        None => serde_json::Value::Null,
    };

    let source = codex_core::workflow_cli::resolve_target(config, &target).await?;
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    let output = codex_core::workflow_cli::run(
        config,
        &source,
        args_value,
        resume,
        user_instructions_provider,
    )
    .await?;

    println!("run ID: {}", output.run_id);
    for line in &output.narration {
        println!("{line}");
    }
    if let Some(text) = &output.output_text {
        println!("{text}");
    }
    if let Some(error) = &output.error_text {
        anyhow::bail!("workflow run {} failed: {error}", output.run_id);
    }

    Ok(())
}

async fn list_workflows(list_args: WorkflowListArgs, config: &Config) -> anyhow::Result<()> {
    let runs = codex_core::workflow_cli::list_runs(config, list_args.limit).await?;
    print!("{}", render_run_list(&runs));
    Ok(())
}

async fn watch_workflow(watch_args: WorkflowWatchArgs, config: &Config) -> anyhow::Result<()> {
    let interactive_output = io::stdout().is_terminal() && !watch_args.json;
    let mut previous = None;
    loop {
        let view = codex_core::workflow_cli::inspect_run(config, &watch_args.run_id).await?;
        if previous.as_ref() != Some(&view) {
            if watch_args.json {
                println!("{}", render_run_watch_json(&view));
            } else if interactive_output {
                print!("\x1b[2J\x1b[H");
                print!("{}", render_run_watch(&view));
            } else {
                if previous.is_some() {
                    println!("---");
                }
                print!("{}", render_run_watch(&view));
            }
            io::stdout().flush()?;
            previous = Some(view.clone());
        }
        if view.terminal {
            return Ok(());
        }
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
            () = tokio::time::sleep(WORKFLOW_WATCH_POLL_INTERVAL) => {}
        }
    }
}

fn render_run_watch_json(view: &RunWatchView) -> String {
    let phases = view
        .phases
        .iter()
        .map(|phase| {
            serde_json::json!({
                "index": phase.index,
                "title": phase.title,
                "status": phase.status,
                "implicit": phase.implicit,
            })
        })
        .collect::<Vec<_>>();
    let nodes = view
        .nodes
        .iter()
        .map(|node| match &node.kind {
            RunWatchNodeKind::Group {
                kind,
                item_count,
                status,
            } => serde_json::json!({
                "id": node.id,
                "parentNodeId": node.parent_node_id,
                "phaseIndex": node.phase_index,
                "kind": "group",
                "groupKind": kind,
                "itemCount": item_count,
                "status": status,
            }),
            RunWatchNodeKind::Agent {
                label,
                model,
                effort,
                child_thread_id,
                status,
                total_tokens,
                tool_call_count,
                returned_null,
                rollout_summary,
            } => serde_json::json!({
                "id": node.id,
                "parentNodeId": node.parent_node_id,
                "phaseIndex": node.phase_index,
                "kind": "agent",
                "label": label,
                "model": model,
                "effort": effort,
                "childThreadId": child_thread_id.as_ref().map(ToString::to_string),
                "status": status,
                "totalTokens": total_tokens,
                "toolCallCount": tool_call_count,
                "returnedNull": returned_null,
                "rolloutSummary": rollout_summary,
            }),
        })
        .collect::<Vec<_>>();
    let unprojected_agents = view
        .unprojected_agents
        .iter()
        .map(|agent| {
            serde_json::json!({
                "ordinal": agent.ordinal,
                "threadId": agent.thread_id.to_string(),
                "rolloutSummary": agent.rollout_summary,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "runId": view.run_id,
        "name": view.name,
        "status": view.status,
        "terminal": view.terminal,
        "budget": view.budget.map(|budget| serde_json::json!({
            "spent": budget.spent,
            "total": budget.total,
        })),
        "phases": phases,
        "nodes": nodes,
        "unprojectedAgents": unprojected_agents,
        "warnings": view.warnings,
    })
    .to_string()
}

fn render_run_watch(view: &RunWatchView) -> String {
    let mut output = format!(
        "workflow {} ({}) [{}]\n",
        single_line(&view.name),
        view.run_id,
        view.status
    );
    if let Some(budget) = view.budget {
        output.push_str(&render_budget(budget));
    }
    for warning in &view.warnings {
        output.push_str(&format!("warning: {}\n", single_line(warning)));
    }

    let by_id = view
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut children = HashMap::<u64, Vec<u64>>::new();
    for node in &view.nodes {
        if let Some(parent_node_id) = node.parent_node_id
            && by_id.contains_key(&parent_node_id)
        {
            children.entry(parent_node_id).or_default().push(node.id);
        }
    }
    let mut visited = HashSet::new();
    for phase in &view.phases {
        let icon = match phase.status.as_str() {
            "pending" => "○",
            "active" => "●",
            "completed" => "✓",
            _ => "?",
        };
        output.push_str(&format!(
            "{icon} phase {}: {} [{}]\n",
            phase.index.saturating_add(1),
            single_line(&phase.title),
            phase.status
        ));
        let roots = view
            .nodes
            .iter()
            .filter(|node| {
                node.phase_index == phase.index
                    && node
                        .parent_node_id
                        .and_then(|parent| by_id.get(&parent))
                        .is_none_or(|parent| parent.phase_index != phase.index)
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();
        for (index, node_id) in roots.iter().enumerate() {
            render_watch_node(
                *node_id,
                &by_id,
                &children,
                "  ",
                index + 1 == roots.len(),
                /*depth*/ 0,
                &mut visited,
                &mut output,
            );
        }
    }
    for node in &view.nodes {
        if !visited.contains(&node.id) {
            render_watch_node(
                node.id,
                &by_id,
                &children,
                "  ",
                true,
                /*depth*/ 0,
                &mut visited,
                &mut output,
            );
        }
    }
    if view.phases.is_empty() && view.nodes.is_empty() {
        output.push_str("  (no progress topology yet)\n");
    }
    if !view.unprojected_agents.is_empty() {
        output.push_str("journal-linked agents (progress fallback):\n");
        for agent in &view.unprojected_agents {
            output.push_str(&format!(
                "  └─ agent {} · thread {}\n",
                agent.ordinal.saturating_add(1),
                agent.thread_id
            ));
            if let Some(summary) = &agent.rollout_summary {
                output.push_str(&format!("     {}\n", single_line(summary)));
            }
        }
    }
    output
}

fn render_budget(budget: RunWatchBudget) -> String {
    match budget.total {
        Some(total) => format!("budget: {}/{total} weighted tokens\n", budget.spent),
        None => format!("budget: {} weighted tokens (unmetered)\n", budget.spent),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_watch_node(
    node_id: u64,
    by_id: &BTreeMap<u64, &RunWatchNode>,
    children: &HashMap<u64, Vec<u64>>,
    prefix: &str,
    is_last: bool,
    depth: usize,
    visited: &mut HashSet<u64>,
    output: &mut String,
) {
    let branch = if is_last { "└─" } else { "├─" };
    if depth >= MAX_WATCH_TREE_DEPTH {
        output.push_str(&format!("{prefix}{branch} … depth limit reached\n"));
        return;
    }
    if !visited.insert(node_id) {
        output.push_str(&format!("{prefix}{branch} node {node_id} (cycle)\n"));
        return;
    }
    let Some(node) = by_id.get(&node_id) else {
        return;
    };
    match &node.kind {
        RunWatchNodeKind::Group {
            kind,
            item_count,
            status,
        } => output.push_str(&format!(
            "{prefix}{branch} {kind} group ({item_count} items) [{status}]\n"
        )),
        RunWatchNodeKind::Agent {
            label,
            model,
            effort,
            child_thread_id,
            status,
            total_tokens,
            tool_call_count,
            returned_null,
            rollout_summary,
        } => {
            output.push_str(&format!(
                "{prefix}{branch} {} [{status}] · {total_tokens} tokens · {tool_call_count} tools",
                single_line(label)
            ));
            if let Some(model) = model {
                output.push_str(&format!(" · {}", single_line(model)));
            }
            if let Some(effort) = effort {
                output.push_str(&format!("/{effort}"));
            }
            if let Some(thread_id) = child_thread_id {
                output.push_str(&format!(" · thread {thread_id}"));
            }
            if *returned_null {
                output.push_str(" · null");
            }
            output.push('\n');
            if let Some(summary) = rollout_summary {
                output.push_str(&format!("{prefix}   {}\n", single_line(summary)));
            }
        }
    }

    let child_ids = children.get(&node_id).map(Vec::as_slice).unwrap_or(&[]);
    let child_prefix = format!("{prefix}{} ", if is_last { "  " } else { "│ " });
    for (index, child_id) in child_ids.iter().enumerate() {
        render_watch_node(
            *child_id,
            by_id,
            children,
            &child_prefix,
            index + 1 == child_ids.len(),
            depth.saturating_add(1),
            visited,
            output,
        );
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_list_limit(raw: &str) -> Result<usize, String> {
    let limit = raw
        .parse::<usize>()
        .map_err(|_| format!("limit must be an integer from 1 to {MAX_WORKFLOW_LIST_LIMIT}"))?;
    if !(1..=MAX_WORKFLOW_LIST_LIMIT).contains(&limit) {
        return Err(format!("limit must be from 1 to {MAX_WORKFLOW_LIST_LIMIT}"));
    }
    Ok(limit)
}

fn render_run_list(runs: &[RunSummary]) -> String {
    const RUN_ID_WIDTH: usize = 36;
    const STATUS_WIDTH: usize = 9;
    const NAME_WIDTH: usize = 24;
    const CREATED_AT_WIDTH: usize = 24;
    const PARENT_WIDTH: usize = 36;
    const RESUMED_FROM_WIDTH: usize = 36;
    const SCRIPT_HASH_WIDTH: usize = 24;
    const SCRIPT_PATH_WIDTH: usize = 72;

    let mut output = format!(
        "{:<RUN_ID_WIDTH$}  {:<STATUS_WIDTH$}  {:<NAME_WIDTH$}  {:<CREATED_AT_WIDTH$}  {:<PARENT_WIDTH$}  {:<RESUMED_FROM_WIDTH$}  {:<SCRIPT_HASH_WIDTH$}  {}\n",
        "RUN ID",
        "STATUS",
        "NAME",
        "CREATED AT",
        "PARENT RUN ID",
        "RESUMED FROM RUN ID",
        "SCRIPT HASH",
        "SCRIPT PATH"
    );
    if runs.is_empty() {
        output.push_str("(no workflow runs)\n");
        return output;
    }

    for run in runs {
        let run_id = table_cell(&run.run_id, RUN_ID_WIDTH);
        let status = table_cell(&run.status, STATUS_WIDTH);
        let name = table_cell(&run.name, NAME_WIDTH);
        let created_at = table_cell(&run.created_at, CREATED_AT_WIDTH);
        let parent = table_cell(run.parent_run_id.as_deref().unwrap_or("-"), PARENT_WIDTH);
        let resumed_from = table_cell(
            run.resumed_from_run_id.as_deref().unwrap_or("-"),
            RESUMED_FROM_WIDTH,
        );
        let script_hash = table_cell(&run.script_hash, SCRIPT_HASH_WIDTH);
        let script_path = table_cell(&run.script_path, SCRIPT_PATH_WIDTH);
        output.push_str(&format!(
            "{run_id:<RUN_ID_WIDTH$}  {status:<STATUS_WIDTH$}  {name:<NAME_WIDTH$}  {created_at:<CREATED_AT_WIDTH$}  {parent:<PARENT_WIDTH$}  {resumed_from:<RESUMED_FROM_WIDTH$}  {script_hash:<SCRIPT_HASH_WIDTH$}  {script_path}\n"
        ));
    }
    output
}

fn table_cell(value: &str, width: usize) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut characters = sanitized.chars();
    let prefix = characters.by_ref().take(width).collect::<String>();
    if characters.next().is_none() {
        return prefix;
    }

    prefix
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
