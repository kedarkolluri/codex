//! Workflow host tool skeleton (P0-host-tool-skeleton).
//!
//! This is a clone of [`super::execute_handler::CodeModeExecuteHandler`] adapted
//! to run a *workflow* script instead of a model-authored code-mode `exec`
//! program. The only structural differences from plain code-mode exec are:
//!
//! 1. The handler is gated behind [`Feature::Workflow`] — it is unreachable
//!    unless the feature is enabled.
//! 2. Before touching the isolate the raw source is validated with
//!    [`codex_code_mode::parse_workflow_meta`]; a script without a valid static
//!    `export const meta = { ... }` manifest is rejected up front, so no isolate
//!    execution ever runs for an invalid workflow.
//! 3. The validated body is then submitted to a *fresh* code-mode isolate via
//!    the shared `code_mode_service.execute` / `run_runtime` path and its
//!    top-level result is returned.
//!
//! Everything downstream of "run the body once" — `agent()`, journal, budget,
//! worktree isolation, and determinism hardening — is intentionally deferred to
//! later phases. This skeleton shares the code-mode service and does NOT fork
//! the runtime, so existing code-mode behavior is unaffected.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_features::Feature;
use codex_features::Features;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

use super::CodeModeService;
use super::ExecContext;
use super::WORKFLOW_TOOL_NAME;
use super::handle_runtime_response;
use super::is_workflow_tool_name;

/// Hard cap (in bytes) on any error string this handler surfaces to the model
/// as a tool result.
///
/// Defense-in-depth: even though [`codex_code_mode::parse_workflow_meta`] now
/// bounds the identifiers/numbers it echoes, an error can still originate from
/// several layers (the exec-source parser, the isolate service, the runtime
/// response adapter). No single error returned from the workflow tool path
/// should be able to balloon the model context, so every model-visible error is
/// hard-truncated at a UTF-8 boundary with a marker.
const MAX_MODEL_ERROR_BYTES: usize = 2048;

/// Marker appended to a truncated error. Its byte length is reserved inside the
/// [`MAX_MODEL_ERROR_BYTES`] budget so the final string never exceeds the cap.
const ERROR_TRUNCATION_MARKER: &str = "… [error truncated]";

/// Truncate a model-visible error message so its total length never exceeds the
/// hard cap [`MAX_MODEL_ERROR_BYTES`]. The marker length is reserved inside the
/// budget (truncate to `cap - marker_len` at a UTF-8 char boundary), so the
/// returned string — prefix plus marker — is guaranteed `<= MAX_MODEL_ERROR_BYTES`.
fn truncate_model_error(message: String) -> String {
    if message.len() <= MAX_MODEL_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_MODEL_ERROR_BYTES.saturating_sub(ERROR_TRUNCATION_MARKER.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ERROR_TRUNCATION_MARKER}", &message[..end])
}

/// Hard-bound the runtime error text carried by a successful workflow run before
/// it reaches the model.
///
/// A script failure does NOT come back as [`FunctionCallError`]; it returns as
/// `Ok(RuntimeResponse::Result { error_text: Some(..) })`, which
/// [`handle_runtime_response`] renders into model-visible output subject only to
/// the workflow's *own* `max_output_tokens` budget (which the script can raise).
/// That bypasses [`bound_model_error`], so the same [`MAX_MODEL_ERROR_BYTES`] cap
/// must be applied here independently, before the response is rendered.
fn bound_runtime_error(response: &mut RuntimeResponse) {
    if let RuntimeResponse::Result {
        error_text: Some(text),
        ..
    } = response
    {
        let bounded = truncate_model_error(std::mem::take(text));
        *text = bounded;
    }
}

/// Hard-bound a [`FunctionCallError`] before it reaches the model. Only the
/// message payload is truncated; the error variant is preserved.
fn bound_model_error(error: FunctionCallError) -> FunctionCallError {
    match error {
        FunctionCallError::RespondToModel(message) => {
            FunctionCallError::RespondToModel(truncate_model_error(message))
        }
        FunctionCallError::Fatal(message) => {
            FunctionCallError::Fatal(truncate_model_error(message))
        }
    }
}

/// Configure the workflow run's shared [`RolloutBudget`] from `args.budget.total`.
///
/// A workflow's `budget` contract is pure output-token spend (spec §8): the
/// ceiling is installed with `sampling_token_weight = 1.0` (count output),
/// `prefill_token_weight = 0.0` (ignore input), and no mid-run reminders, so the
/// tree-wide `weighted_tokens_used` counter measures pure output-token spend.
/// The limit is `args.budget.total`.
///
/// Configuration goes through the resettable budget cell
/// ([`RolloutBudget::configure`]), not the `OnceLock` path, so a nested/reused
/// workflow run can re-set the limit. The `AgentControl.rollout_budget` is an
/// `Arc` shared by the root thread and every cloned sub-agent control handle, so
/// this single call meters every subagent spawned through the workflow root.
///
/// A TOP-LEVEL workflow run always first returns the shared cell to the SESSION
/// baseline (`config.rollout_budget`), then applies its own `args.budget.total` if
/// present.
///
/// Returning to the baseline is the finding #4 fix, reconciled with the session
/// budget model: an ABSENT `budget` must NOT leave a prior run's per-run limit
/// installed on the shared cell (which would make an "unmetered" run wrongly reject
/// `agent()`), but it must ALSO NOT wipe a genuine session-configured ceiling — a
/// session budget still gates workflow agents (UAT-5). So:
///  - `session_baseline = Some(cfg)` → reinstall the session ceiling (a no-op that
///    preserves accrued spend when it is already installed), then apply any run
///    `budget`.
///  - `session_baseline = None` → reset to unmetered, clearing any leftover per-run
///    limit, then apply any run `budget`.
///
/// An EXPLICIT `total` (including `0`) installs a real ceiling on top of the
/// baseline. `total == 0` is a genuine zero budget that rejects the first `agent()`
/// at `remaining() == 0`; negative totals are clamped to `0` (finding #3).
fn configure_workflow_budget(
    session: &crate::session::session::Session,
    args: &serde_json::Value,
    session_baseline: Option<&crate::config::RolloutBudgetConfig>,
) {
    apply_workflow_budget(
        session.services.agent_control.rollout_budget(),
        args,
        session_baseline,
    );
}

/// Pure budget-application logic for a top-level run (see [`configure_workflow_budget`]),
/// split out so it can be unit-tested against a bare [`RolloutBudget`].
fn apply_workflow_budget(
    budget: &crate::rollout_budget::RolloutBudget,
    args: &serde_json::Value,
    session_baseline: Option<&crate::config::RolloutBudgetConfig>,
) {
    match session_baseline {
        Some(baseline) => budget.configure(baseline.clone()),
        None => budget.reset(),
    }
    if let WorkflowBudgetSpec::Limit(total) = workflow_budget_spec(args) {
        budget.configure(crate::rollout_budget::workflow_output_weight_config(total));
    }
}

/// Whether a workflow invocation declares a budget ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowBudgetSpec {
    /// No usable `budget.total`: the run is unmetered (the cell is reset).
    Absent,
    /// An explicit, non-negative ceiling. `0` is a real zero budget (reject the
    /// first `agent()`), not "unmetered".
    Limit(i64),
}

/// Classify `args.budget.total` into a [`WorkflowBudgetSpec`].
///
/// Distinguishing ABSENT (no `budget` key, or `budget` with no integer `total`)
/// from an explicit `total == 0` is the whole point: absent means unmetered, while
/// `{ budget: { total: 0 } }` is a real zero ceiling that must reject the first
/// `agent()` (finding #3). A negative total is invalid and clamped to `0` rather
/// than silently treated as unmetered.
fn workflow_budget_spec(args: &serde_json::Value) -> WorkflowBudgetSpec {
    match args
        .get("budget")
        .and_then(|budget| budget.get("total"))
        .and_then(serde_json::Value::as_i64)
    {
        Some(total) => WorkflowBudgetSpec::Limit(total.max(0)),
        None => WorkflowBudgetSpec::Absent,
    }
}

/// Where a workflow run sits in the nesting tree, threaded into
/// [`run_workflow_source`] so the ledger records both facts the moment the run's
/// isolate cell is created: its `parent_run_id` (for the child's parent linkage)
/// and its `workflow()` nesting `depth` (for the one-level depth guard).
pub(crate) struct WorkflowRunLineage {
    /// Run id of the workflow whose isolate spawned this one, or `None` for a
    /// top-level (model-callable) run.
    pub(crate) parent_run_id: Option<String>,
    /// `workflow()` nesting depth: 0 for a top-level model-callable run, 1 for a
    /// run spawned by a depth-0 workflow, …
    pub(crate) depth: i32,
}

/// One recorded workflow run → parent linkage. The journal (Phase 3) is the
/// durable home for this, but the run→parent edge is threaded now so a nested
/// `workflow()` run's `parent_run_id` is set the moment it is spawned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowRunLink {
    /// Host-minted run id of this run (`workflow.runId`).
    pub(crate) run_id: String,
    /// Run id of the workflow whose isolate spawned this one, or `None` for a
    /// top-level (model-callable) workflow run.
    pub(crate) parent_run_id: Option<String>,
}

/// One workflow run's isolate-cell bookkeeping: the run id executing in the cell
/// plus its `workflow()` nesting depth (0 for a top-level model-callable run, 1
/// for a run spawned by a depth-0 workflow, …). The depth is what the one-level
/// nesting guard ([`run_workflow_by_name`]) reads to reject a workflow() call that
/// would create a second nesting level.
#[derive(Clone, Debug)]
struct CellRun {
    run_id: String,
    depth: i32,
}

/// In-memory ledger of workflow runs keyed by the isolate cell executing them.
///
/// It serves two jobs while the durable journal is still Phase 3:
/// 1. `cell_id → {run_id, depth}` so a nested `workflow()` call (which arrives at
///    the host tagged with the *parent* cell id) can recover the parent run id to
///    set `parent_run_id` on the child run and the parent's nesting depth to gate
///    the one-level depth guard.
/// 2. an append-only list of `(run_id, parent_run_id)` links for observability /
///    the eventual journal `run_meta` line.
#[derive(Default)]
pub(crate) struct WorkflowRunLedger {
    cell_runs: Mutex<HashMap<CellId, CellRun>>,
    links: Mutex<Vec<WorkflowRunLink>>,
}

impl WorkflowRunLedger {
    /// Record a freshly-minted run executing in `cell_id` at nesting `depth`,
    /// linked to `parent_run_id`. Called the moment the isolate cell is created
    /// (before the body runs) so a `workflow()` fired *during* the body finds both
    /// the parent run id and the parent's depth.
    fn register_run(
        &self,
        cell_id: CellId,
        run_id: String,
        parent_run_id: Option<String>,
        depth: i32,
    ) {
        if let Ok(mut cell_runs) = self.cell_runs.lock() {
            cell_runs.insert(
                cell_id,
                CellRun {
                    run_id: run_id.clone(),
                    depth,
                },
            );
        }
        if let Ok(mut links) = self.links.lock() {
            links.push(WorkflowRunLink {
                run_id,
                parent_run_id,
            });
        }
    }

    /// The run id of the workflow executing in `cell_id`, if any — the parent run
    /// id for a `workflow()` call made from that cell.
    pub(crate) fn parent_run_id_for_cell(&self, cell_id: &CellId) -> Option<String> {
        self.cell_runs
            .lock()
            .ok()
            .and_then(|cell_runs| cell_runs.get(cell_id).map(|run| run.run_id.clone()))
    }

    /// The `workflow()` nesting depth of the run executing in `cell_id`, if known.
    /// A `workflow()` call made from this cell nests one level below this depth.
    pub(crate) fn depth_for_cell(&self, cell_id: &CellId) -> Option<i32> {
        self.cell_runs
            .lock()
            .ok()
            .and_then(|cell_runs| cell_runs.get(cell_id).map(|run| run.depth))
    }

    /// Drop the `cell_id → {run_id, depth}` entry once a cell reaches a terminal
    /// state so the map does not grow across a long session. The append-only link
    /// list is retained (it is the run→parent record).
    pub(crate) fn forget_cell(&self, cell_id: &CellId) {
        if let Ok(mut cell_runs) = self.cell_runs.lock() {
            cell_runs.remove(cell_id);
        }
    }

    /// Snapshot of every recorded run→parent link, in registration order. Consumed by tests now and
    /// by the Phase-3 journal `run_meta` writer, which persists each run's `parent_run_id`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn links(&self) -> Vec<WorkflowRunLink> {
        self.links
            .lock()
            .map(|links| links.clone())
            .unwrap_or_default()
    }
}

/// Result of running a workflow body once in a fresh isolate, carrying the
/// bits `handle_runtime_response` needs to render the model-facing output.
#[derive(Debug)]
pub(crate) struct WorkflowRunOutput {
    pub(crate) response: RuntimeResponse,
    pub(crate) max_output_tokens: Option<usize>,
    pub(crate) started_at: Instant,
    /// The isolate cell the body ran in. A nested run needs this to terminate a
    /// cell that only `Yielded` (never reached a terminal `Result`) so its isolate
    /// and dispatch/ledger entry do not leak (finding #7).
    pub(crate) cell_id: CellId,
}

/// Reject the call unless the `workflow` feature is enabled. This is what makes
/// the handler unreachable when [`Feature::Workflow`] is off.
pub(crate) fn ensure_workflow_enabled(features: &Features) -> Result<(), FunctionCallError> {
    if features.enabled(Feature::Workflow) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "`{WORKFLOW_TOOL_NAME}` requires the `workflow` feature to be enabled"
        )))
    }
}

/// Validate the leading `export const meta = { ... }` manifest WITHOUT executing
/// (or even reading past) the workflow body. A script with a missing or invalid
/// `meta` is rejected here, before any isolate execution.
pub(crate) fn validate_workflow_meta(code: &str) -> Result<(), FunctionCallError> {
    codex_code_mode::parse_workflow_meta(code)
        .map(|_meta| ())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid workflow `meta` manifest: {err}"))
        })
}

/// Feature-gate, parse+validate the manifest, then run the workflow body exactly
/// once in a fresh code-mode isolate via the shared service.
///
/// Shared by the [`CodeModeWorkflowHandler`] tool path and the integration test
/// so both drive the identical run-body-once sequence.
pub(crate) async fn run_workflow_source(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
    args: serde_json::Value,
    lineage: WorkflowRunLineage,
) -> Result<WorkflowRunOutput, FunctionCallError> {
    ensure_workflow_enabled(features)?;

    // The workflow source is admitted identically to any code-mode program: an
    // optional leading `// @exec:` pragma followed by the ES-module body.
    let exec_args =
        codex_code_mode::parse_exec_source(source).map_err(FunctionCallError::RespondToModel)?;

    // Reject scripts without a valid static `meta` manifest before we ever touch
    // the isolate.
    validate_workflow_meta(&exec_args.code)?;

    // Mint the run id host-side in Rust (uuid v7, `items.rs` pattern) — never in
    // the isolate — so `workflow.runId` is a host-authored value the script can
    // read but never derive. Exposed read-only inside the fresh isolate.
    let run_id = uuid::Uuid::now_v7().to_string();

    let started_at = Instant::now();
    let started_cell = service
        .execute(codex_code_mode::ExecuteRequest {
            tool_call_id: call_id,
            enabled_tools,
            source: exec_args.code.clone(),
            yield_time_ms: exec_args.yield_time_ms,
            max_output_tokens: exec_args.max_output_tokens,
            // Explicit workflow invocation mode: authorizes the workflow-only
            // narrator globals for this fresh isolate. Plain code-mode exec
            // leaves this `false`.
            workflow: true,
            // Invocation JSON injected read-only as the `args` global, and the
            // host-minted run id exposed read-only as `workflow.runId`.
            args: Some(args),
            run_id: Some(run_id.clone()),
        })
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let cell_id = started_cell.cell_id.clone();
    // Record the run→parent link BEFORE the body runs: a `workflow()` call made
    // during this body arrives at the host tagged with THIS cell id, and must be
    // able to recover this run id as its `parent_run_id`.
    service.workflow_run_ledger().register_run(
        cell_id.clone(),
        run_id.clone(),
        lineage.parent_run_id,
        lineage.depth,
    );
    service.mark_cell_ready_for_dispatch(&cell_id);
    let response = started_cell
        .initial_response()
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    // Yielded cells keep running; the terminal lifecycle is only closed here when
    // the first response also ended the runtime.
    if !matches!(response, RuntimeResponse::Yielded { .. }) {
        service.finish_cell_dispatch(&cell_id);
    }

    Ok(WorkflowRunOutput {
        response,
        max_output_tokens: exec_args.max_output_tokens,
        started_at,
        cell_id,
    })
}

/// The hard `workflow()` nesting ceiling: exactly ONE level, ALWAYS.
///
/// Per the spec non-goal "`workflow()` is one level deep only", this is a fixed
/// policy that must NOT track the configurable `agent_max_depth` (which governs
/// `agent()` thread nesting, not workflow re-entry). A top-level (depth-0) run may
/// spawn a nested workflow (depth 1); a depth-1 run may not nest further.
const MAX_WORKFLOW_NESTING_DEPTH: i32 = 1;

/// One-level `workflow()` nesting decision (`P2-workflow-depth-guard`, spec §4/§6).
///
/// A `workflow()` call made from a run already at `parent_depth` would create a
/// child run at [`next_spawn_depth`]`(parent_depth)` — the same saturating
/// depth-increment the thread-spawn gate uses. Unlike `agent()` thread nesting,
/// the workflow re-entry ceiling is HARD-CAPPED at [`MAX_WORKFLOW_NESTING_DEPTH`]
/// (exactly one level) regardless of `agent_max_depth`: a depth-1 child (top-level
/// parent) is admitted, a depth-2 child (an already-nested parent) is always
/// rejected — even when `agent_max_depth` is raised (finding #6 / CLAIM-9).
///
/// Returns `Ok(child_depth)` when the nested run is admitted, or
/// `Err(child_depth)` when it would exceed the one-level limit — the caller turns
/// the `Err` into a `workflow()` promise rejection rather than a silent hang.
fn admit_nested_workflow_depth(parent_depth: i32) -> Result<i32, i32> {
    use crate::agent::next_spawn_depth;

    let child_depth = next_spawn_depth(parent_depth);
    if child_depth > MAX_WORKFLOW_NESTING_DEPTH {
        Err(child_depth)
    } else {
        Ok(child_depth)
    }
}

/// Host handler for `RuntimeEvent::WorkflowCall` (`P2-workflow-registry-reenter`).
///
/// Resolves `name` against the layered core-workflows registry, loads the named
/// saved workflow's script, and re-enters the runtime ONE level deep as a nested
/// run via [`run_workflow_source`] (a fresh isolate through the shared code-mode
/// service), returning the nested run's top-level result to the `workflow()`
/// promise. The nested run's agents spawn through the same per-turn dispatch
/// worker/scheduler as the parent, so depth/registry/concurrency accounting flows
/// (depth enforcement is `P2-workflow-depth-guard`); its budget is reconfigured
/// through the resettable budget cell ([`configure_workflow_budget`]).
///
/// Outcome mapping (mirrors the `agent()` seam contract):
/// - a name that does not resolve in the registry, an unreadable script, a
///   feature/parse rejection, or a nested *script error* -> [`AgentSpawnOutcome::Rejected`]
///   (a JS throw), so a failure surfaces on the `workflow()` promise rather than
///   hanging silently;
/// - a nested run that finished but produced no textual result -> [`AgentSpawnOutcome::Failed`]
///   (JS `null`);
/// - success -> [`AgentSpawnOutcome::Completed`] carrying the nested run's joined
///   top-level text.
pub(crate) async fn run_workflow_by_name(
    exec: &ExecContext,
    parent_cell_id: &CellId,
    name: &str,
    args: Option<serde_json::Value>,
) -> codex_code_mode::AgentSpawnOutcome {
    use codex_code_mode::AgentSpawnOutcome;

    let service = &exec.session.services.code_mode_service;
    let ledger = service.workflow_run_ledger();
    let parent_run_id = ledger.parent_run_id_for_cell(parent_cell_id);

    // One-level nesting depth guard (spec §4/§6). The parent cell's run was
    // registered with its `workflow()` nesting depth (0 for a top-level
    // model-callable run); a `workflow()` fired from it nests one level below. The
    // ceiling is a HARD ONE LEVEL, independent of the configurable
    // `agent_max_depth`: a first nested run is admitted, a second nesting level is
    // rejected as a JS throw on the `workflow()` promise — never a silent hang. The
    // guard runs before any registry resolution, file read, or budget
    // reconfiguration so a too-deep call is rejected cheaply and touches no shared
    // state.
    let parent_depth = ledger.depth_for_cell(parent_cell_id).unwrap_or(0);
    let child_depth = match admit_nested_workflow_depth(parent_depth) {
        Ok(child_depth) => child_depth,
        Err(child_depth) => {
            return AgentSpawnOutcome::Rejected(format!(
                "workflow('{name}') exceeds the one-level nesting limit \
                 (depth {child_depth} > {MAX_WORKFLOW_NESTING_DEPTH}); \
                 workflow() may nest only one level deep"
            ));
        }
    };

    // Resolve the named saved workflow from the layered workflow roots (spec §9):
    // project (`<cwd>/.codex/workflows`) > personal (`$HOME/.agents/workflows`) >
    // `$CODEX_HOME/workflows`. Discovery only static-parses each candidate's
    // leading `meta` literal; no body is ever executed during resolution.
    // Project root (`<cwd>/.codex/workflows`) is derived from the turn's selected (primary)
    // environment cwd.
    let cwd = exec
        .turn
        .environments
        .primary()
        .and_then(|environment| environment.cwd().to_abs_path().ok());
    let roots = codex_core_workflows::workflow_roots(
        cwd.as_deref(),
        dirs::home_dir().as_deref(),
        Some(exec.turn.config.codex_home.as_path()),
    );
    let registry = codex_core_workflows::load_workflows_from_roots(roots).await;
    let Some(metadata) = registry.resolve_by_name(name) else {
        return AgentSpawnOutcome::Rejected(format!(
            "workflow('{name}') did not resolve to a saved workflow in the registry"
        ));
    };

    // Bound the EXECUTION read (finding #8). Discovery only reads a bounded meta
    // prefix, but execution needs the whole body — so an unbounded `read_to_string`
    // of a small-manifest/huge-body file would OOM. Cap the total read here so a
    // pathological saved workflow is rejected with a clear error instead.
    let source = match read_workflow_source_bounded(metadata.path.as_path()).await {
        Ok(source) => source,
        Err(error) => {
            return AgentSpawnOutcome::Rejected(format!(
                "failed to read saved workflow '{name}': {error}"
            ));
        }
    };

    let args_value = args.unwrap_or(serde_json::Value::Null);
    let budget = exec.session.services.agent_control.rollout_budget();
    // Scope the nested run's budget (finding #5): snapshot the parent's config
    // BEFORE reconfiguring the shared cell, then restore it afterward on EVERY exit
    // path so a child's limit never leaks into later parent turns and concurrent
    // children cannot leave a foreign ceiling installed. The shared spend counter
    // is deliberately preserved (never snapshotted), so a nested run's subagent
    // spend still accumulates against the tree-wide budget and a child cannot evade
    // the parent ceiling by nesting.
    let restore_snapshot = budget.snapshot_config();
    configure_nested_workflow_budget(budget, &args_value, restore_snapshot.as_ref());

    // A unique tool-call id for the nested isolate cell (distinct from the parent
    // cell's); deterministic-path code never reads it, so a host-minted uuid is fine.
    let call_id = format!("workflow-nested-{}", uuid::Uuid::now_v7());
    let outcome = match run_workflow_source(
        exec.turn.config.features.get(),
        service,
        call_id,
        // Nested workflows orchestrate via the `agent()`/`parallel()`/`pipeline()`/
        // `workflow()` globals (installed by `workflow: true`); code-mode nested
        // `tool.*` calls are not surfaced into a nested workflow.
        Vec::new(),
        &source,
        args_value,
        WorkflowRunLineage {
            parent_run_id,
            depth: child_depth,
        },
    )
    .await
    {
        Ok(output) => {
            if matches!(output.response, RuntimeResponse::Yielded { .. }) {
                // A nested run that only YIELDED never reached a terminal result: the
                // child isolate/callbacks are still running (finding #7). Do NOT
                // resolve the `workflow()` promise with its partial/null output —
                // terminate the cell to reclaim its isolate and close its
                // dispatch/ledger entry, then reject clearly.
                let _ = service.terminate(output.cell_id.clone()).await;
                service.finish_cell_dispatch(&output.cell_id);
                AgentSpawnOutcome::Rejected(format!(
                    "workflow('{name}') did not run to completion (it yielded); \
                     a nested workflow must complete synchronously"
                ))
            } else {
                workflow_result_outcome(output.response)
            }
        }
        Err(error) => AgentSpawnOutcome::Rejected(error.to_string()),
    };

    // Restore the parent's ceiling regardless of how the nested run ended.
    budget.restore_config(restore_snapshot);
    outcome
}

/// Hard cap on the total byte size of a saved workflow body read for EXECUTION.
///
/// Registry discovery reads only a bounded `meta` prefix, but a nested `workflow()`
/// run must load the whole body — so without a cap a tiny-manifest/huge-body file
/// would materialize unbounded bytes and OOM. 1 MiB is far above any legitimate
/// workflow script.
const MAX_WORKFLOW_SOURCE_BYTES: u64 = 1024 * 1024;

/// Read a saved workflow source bounded to [`MAX_WORKFLOW_SOURCE_BYTES`]. A file
/// larger than the cap (or one that is not valid UTF-8) is rejected with an
/// `InvalidData` error rather than read in full — mirroring the discovery loader's
/// bounded-read hardening, extended to a hard TOTAL-size cap for the execution read.
async fn read_workflow_source_bounded(path: &std::path::Path) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    // Read one byte past the cap so a file sitting exactly at the cap is accepted
    // while anything larger is detected and rejected.
    file.take(MAX_WORKFLOW_SOURCE_BYTES + 1)
        .read_to_end(&mut buf)
        .await?;
    if buf.len() as u64 > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("workflow source exceeds the {MAX_WORKFLOW_SOURCE_BYTES}-byte execution cap"),
        ));
    }
    String::from_utf8(buf)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

/// Configure the shared budget for a NESTED `workflow()` run.
///
/// Unlike the top-level [`configure_workflow_budget`], an ABSENT budget here does
/// NOT reset the cell — a nested child with no `budget` INHERITS the parent's
/// ceiling (its subagents draw from the shared, tree-wide budget). An explicit
/// child ceiling is CLAMPED to the parent's absolute limit so a nested run can only
/// tighten, never RAISE, the tree-wide budget (finding #5).
fn configure_nested_workflow_budget(
    budget: &crate::rollout_budget::RolloutBudget,
    args: &serde_json::Value,
    parent_config: Option<&crate::config::RolloutBudgetConfig>,
) {
    match workflow_budget_spec(args) {
        WorkflowBudgetSpec::Absent => {}
        WorkflowBudgetSpec::Limit(total) => {
            let effective = match parent_config {
                Some(parent) => total.min(parent.limit_tokens),
                None => total,
            };
            budget.configure(crate::rollout_budget::workflow_output_weight_config(
                effective,
            ));
        }
    }
}

/// Map a TERMINAL nested workflow run's [`RuntimeResponse`] onto the
/// [`AgentSpawnOutcome`] the `workflow()` promise settles with.
///
/// The nested run's top-level result is its joined `text(...)` output. A script
/// error (`error_text`) surfaces as [`AgentSpawnOutcome::Rejected`] (a JS throw,
/// never a silent hang); a run that produced no text resolves to
/// [`AgentSpawnOutcome::Failed`] (JS `null`).
///
/// A `Yielded` response is NOT terminal — the caller ([`run_workflow_by_name`])
/// intercepts it, terminates the still-running cell, and rejects, so it never
/// reaches here on the production path. It is mapped to [`AgentSpawnOutcome::Rejected`]
/// defensively rather than being mistaken for a completed run (finding #7).
fn workflow_result_outcome(response: RuntimeResponse) -> codex_code_mode::AgentSpawnOutcome {
    use codex_code_mode::AgentSpawnOutcome;

    let (content_items, error_text) = match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (content_items, error_text),
        RuntimeResponse::Terminated { content_items, .. } => (content_items, None),
        RuntimeResponse::Yielded { .. } => {
            return AgentSpawnOutcome::Rejected(
                "nested workflow run did not complete (it yielded)".to_string(),
            );
        }
    };
    if let Some(error_text) = error_text {
        return AgentSpawnOutcome::Rejected(truncate_model_error(format!(
            "nested workflow run failed: {error_text}"
        )));
    }
    match join_result_text(&content_items) {
        Some(text) => AgentSpawnOutcome::Completed(serde_json::Value::String(text)),
        None => AgentSpawnOutcome::Failed,
    }
}

/// Join the non-empty `text(...)` outputs of a nested workflow run into a single
/// string (image items are ignored), or `None` when the run produced no text —
/// the nested run's top-level result the `workflow()` promise resolves with.
fn join_result_text(
    content_items: &[codex_code_mode::FunctionCallOutputContentItem],
) -> Option<String> {
    let segments = content_items
        .iter()
        .filter_map(|item| match item {
            codex_code_mode::FunctionCallOutputContentItem::InputText { text }
                if !text.trim().is_empty() =>
            {
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

pub(crate) struct CodeModeWorkflowHandler {
    spec: ToolSpec,
    nested_tool_specs: Vec<ToolSpec>,
}

impl CodeModeWorkflowHandler {
    pub(crate) fn new(spec: ToolSpec, nested_tool_specs: Vec<ToolSpec>) -> Self {
        Self {
            spec,
            nested_tool_specs,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        source: String,
        args: serde_json::Value,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let exec = ExecContext { session, turn };
        let enabled_tools =
            codex_tools::collect_code_mode_tool_definitions(&self.nested_tool_specs);
        // Install the output-weight budget ceiling from `args.budget.total` on the
        // shared `AgentControl` before the body runs, so every subagent spawned
        // through the workflow root meters output tokens against the ceiling. The
        // SAME `args` value is threaded into both the budget config and the run —
        // never a hardcoded `null` in the middle of the path — so a real
        // `args.budget.total` takes effect for a top-level run exactly as it does
        // for the nested path. The session baseline (`config.rollout_budget`) is
        // passed so an absent run returns to the session ceiling rather than
        // inheriting a prior run's per-run limit (finding #4).
        let session_baseline = exec.turn.config.rollout_budget.clone();
        configure_workflow_budget(&exec.session, &args, session_baseline.as_ref());
        let mut output = run_workflow_source(
            exec.turn.config.features.get(),
            &exec.session.services.code_mode_service,
            call_id,
            enabled_tools,
            &source,
            args,
            // Top-level (model-callable) workflow run: no parent workflow, nesting
            // depth 0. A `workflow()` fired from its body nests to depth 1
            // (admitted) and no deeper (rejected by the one-level guard).
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
        )
        .await?;
        // Script failures return on the `Ok` path as `RuntimeResponse::Result`
        // carrying `error_text`; bound it here so a workflow cannot raise its own
        // `max_output_tokens` to smuggle an unbounded error into the model output.
        bound_runtime_error(&mut output.response);
        exec.session.services.elicitations.wait_until_clear().await;
        handle_runtime_response(
            &exec,
            output.response,
            output.max_output_tokens,
            output.started_at,
        )
        .await
        .map_err(FunctionCallError::RespondToModel)
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeWorkflowHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeWorkflowHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        // `handle_call` is the model-visible boundary for the workflow tool: every
        // error returned here is rendered into a tool result. Hard-bound the
        // message so no layer (meta parser, exec-source parser, isolate service,
        // response adapter) can surface an unbounded string to the model.
        match payload {
            ToolPayload::Custom { input } if is_workflow_tool_name(&tool_name) => {
                let args = top_level_workflow_args(&input);
                self.execute(session, turn, call_id, input, args)
                    .await
                    .map(boxed_tool_output)
                    .map_err(bound_model_error)
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WORKFLOW_TOOL_NAME} expects raw workflow JavaScript source text"
            ))),
        }
    }
}

/// Build the invocation `args` for a TOP-LEVEL (model-callable) workflow run.
///
/// The model-callable `workflow` tool is a *freeform* tool: its only payload is the
/// raw workflow source (there is no structured args slot the model fills, and thus
/// no `args.budget.total`). A top-level run's budget is meant to come from the
/// workflow's own `meta.budget`, but the parsed workflow meta
/// (`ParsedWorkflowMeta`) does not surface a `budget` field yet — exposing it is an
/// owner-of-`code-mode-protocol` (bridge) change. Until it lands this returns
/// `null`, so a top-level run stays unmetered, while the nested
/// `workflow(name, args)` path already threads a real `args.budget`.
///
/// The value is threaded through [`CodeModeWorkflowHandler::execute`] (rather than a
/// hardcoded `null` inline) so the top-level path is structurally identical to the
/// nested one: the moment a `budget` source is available, the SAME `args` flows into
/// both [`configure_workflow_budget`] and the run.
fn top_level_workflow_args(_source: &str) -> serde_json::Value {
    serde_json::Value::Null
}

impl CoreToolRuntime for CodeModeWorkflowHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::ERROR_TRUNCATION_MARKER;
    use super::FunctionCallError;
    use super::MAX_MODEL_ERROR_BYTES;
    use super::RuntimeResponse;
    use super::WorkflowBudgetSpec;
    use super::WorkflowRunLedger;
    use super::admit_nested_workflow_depth;
    use super::bound_model_error;
    use super::bound_runtime_error;
    use super::join_result_text;
    use super::truncate_model_error;
    use super::workflow_budget_spec;
    use super::workflow_result_outcome;
    use codex_code_mode::AgentSpawnOutcome;
    use codex_code_mode::CellId;
    use codex_code_mode::FunctionCallOutputContentItem;

    /// A `text(...)`-only nested run resolves the `workflow()` promise with the joined text.
    #[test]
    fn workflow_result_outcome_returns_joined_text_on_success() {
        let response = RuntimeResponse::Result {
            cell_id: CellId::new("2".to_string()),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "child-result".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "line-2".to_string(),
                },
            ],
            error_text: None,
        };
        match workflow_result_outcome(response) {
            AgentSpawnOutcome::Completed(serde_json::Value::String(text)) => {
                assert_eq!(text, "child-result\nline-2");
            }
            other => panic!("expected Completed(String), got {other:?}"),
        }
    }

    /// A nested run that produced no text resolves to `Failed` (JS null), never a hang.
    #[test]
    fn workflow_result_outcome_empty_result_is_failed() {
        let response = RuntimeResponse::Result {
            cell_id: CellId::new("2".to_string()),
            content_items: Vec::new(),
            error_text: None,
        };
        assert!(matches!(
            workflow_result_outcome(response),
            AgentSpawnOutcome::Failed
        ));
    }

    /// A nested SCRIPT error surfaces as `Rejected` (a JS throw), not a silent null.
    #[test]
    fn workflow_result_outcome_script_error_is_rejected() {
        let response = RuntimeResponse::Result {
            cell_id: CellId::new("2".to_string()),
            content_items: Vec::new(),
            error_text: Some("boom in child".to_string()),
        };
        match workflow_result_outcome(response) {
            AgentSpawnOutcome::Rejected(message) => {
                assert!(
                    message.contains("nested workflow run failed")
                        && message.contains("boom in child"),
                    "reason must name the nested failure: {message}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// `join_result_text` joins non-empty text items and ignores blank ones; empty -> None.
    #[test]
    fn join_result_text_filters_blank_and_joins() {
        let items = vec![
            FunctionCallOutputContentItem::InputText {
                text: "a".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "   ".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "b".to_string(),
            },
        ];
        assert_eq!(join_result_text(&items), Some("a\nb".to_string()));
        assert_eq!(join_result_text(&[]), None);
    }

    /// The ledger records the run→parent link and recovers the parent run id by cell, and
    /// `forget_cell` drops only the cell→run mapping (the append-only links survive).
    #[test]
    fn workflow_run_ledger_records_parent_linkage() {
        let ledger = WorkflowRunLedger::default();
        let parent_cell = CellId::new("1".to_string());
        let child_cell = CellId::new("2".to_string());

        ledger.register_run(parent_cell.clone(), "run-parent".to_string(), None, 0);
        // The nested run recovers its parent from the PARENT cell it was called from.
        assert_eq!(
            ledger.parent_run_id_for_cell(&parent_cell),
            Some("run-parent".to_string())
        );
        // The parent run's nesting depth is recoverable by cell so a `workflow()`
        // call from it can compute the child's depth for the one-level guard.
        assert_eq!(ledger.depth_for_cell(&parent_cell), Some(0));
        ledger.register_run(
            child_cell.clone(),
            "run-child".to_string(),
            ledger.parent_run_id_for_cell(&parent_cell),
            crate::agent::next_spawn_depth(ledger.depth_for_cell(&parent_cell).unwrap_or(0)),
        );
        // The child run registered at depth 1 (one level below the depth-0 parent).
        assert_eq!(ledger.depth_for_cell(&child_cell), Some(1));

        let links = ledger.links();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].run_id, "run-parent");
        assert_eq!(links[0].parent_run_id, None);
        assert_eq!(links[1].run_id, "run-child");
        assert_eq!(
            links[1].parent_run_id,
            Some("run-parent".to_string()),
            "the child run's parent_run_id is set to the calling run"
        );

        // Forgetting a closed cell drops the cell→run entry but keeps the recorded links.
        ledger.forget_cell(&parent_cell);
        assert_eq!(ledger.parent_run_id_for_cell(&parent_cell), None);
        assert_eq!(ledger.links().len(), 2);
    }

    /// The one-level nesting guard admits a depth-1 nested run (top-level parent)
    /// and rejects the depth-2 second nesting level. This is the
    /// `P2-workflow-depth-guard` acceptance: one level ok, two levels throws.
    #[test]
    fn nested_workflow_depth_admits_one_level_and_rejects_two() {
        // Depth-0 (top-level, model-callable) parent → child depth 1: admitted.
        assert_eq!(admit_nested_workflow_depth(0), Ok(1));

        // Depth-1 (already-nested) parent → child depth 2: rejected (second level).
        assert_eq!(admit_nested_workflow_depth(1), Err(2));
    }

    /// Finding #6 / CLAIM-9: the `workflow()` nesting ceiling is a HARD one level
    /// regardless of `agent_max_depth`. A depth-2 nested run is ALWAYS rejected —
    /// the guard does not read `agent_max_depth`, so raising it (e.g. to 2 or 3)
    /// cannot admit a second nesting level.
    #[test]
    fn nested_workflow_depth_is_always_capped_at_one_level() {
        // Depth-1 parent → child depth 2 is rejected no matter the (removed)
        // agent_max_depth knob; there is no argument by which depth 2 is admitted.
        assert_eq!(admit_nested_workflow_depth(1), Err(2));
        // A deeper parent is likewise always rejected.
        assert_eq!(admit_nested_workflow_depth(2), Err(3));
        // Only depth-0 → depth-1 is ever admitted.
        assert_eq!(admit_nested_workflow_depth(0), Ok(1));
    }

    #[test]
    fn workflow_budget_spec_reads_positive_total_from_args() {
        let args = serde_json::json!({ "budget": { "total": 500_000 }, "input": "x" });
        assert_eq!(
            workflow_budget_spec(&args),
            WorkflowBudgetSpec::Limit(500_000)
        );
    }

    /// An ABSENT budget (no `budget` key, or a `budget` without an integer `total`)
    /// is unmetered — distinct from an explicit `total == 0` ceiling.
    #[test]
    fn workflow_budget_spec_absent_for_null_or_missing_budget() {
        assert_eq!(
            workflow_budget_spec(&serde_json::Value::Null),
            WorkflowBudgetSpec::Absent
        );
        assert_eq!(
            workflow_budget_spec(&serde_json::json!({})),
            WorkflowBudgetSpec::Absent
        );
        assert_eq!(
            workflow_budget_spec(&serde_json::json!({ "budget": {} })),
            WorkflowBudgetSpec::Absent
        );
        // A non-integer total carries no usable ceiling → Absent (unmetered).
        assert_eq!(
            workflow_budget_spec(&serde_json::json!({ "budget": { "total": "500" } })),
            WorkflowBudgetSpec::Absent
        );
    }

    /// Finding #3: an explicit `total == 0` is a REAL zero ceiling (not "unmetered"),
    /// and a negative total is invalid and clamped to `0` rather than treated as
    /// unmetered.
    #[test]
    fn workflow_budget_spec_zero_is_a_real_ceiling_and_negatives_clamp() {
        assert_eq!(
            workflow_budget_spec(&serde_json::json!({ "budget": { "total": 0 } })),
            WorkflowBudgetSpec::Limit(0)
        );
        assert_eq!(
            workflow_budget_spec(&serde_json::json!({ "budget": { "total": -1 } })),
            WorkflowBudgetSpec::Limit(0)
        );
    }

    /// Finding #4 (reconciled with the session-budget model): a TOP-LEVEL run with an
    /// absent budget returns the shared cell to the SESSION baseline, clearing a
    /// leftover per-run limit while PRESERVING a session-configured ceiling.
    #[test]
    fn top_level_absent_budget_returns_to_session_baseline() {
        use crate::config::RolloutBudgetConfig;
        use crate::rollout_budget::RolloutBudget;
        use crate::rollout_budget::workflow_output_weight_config;

        let baseline = RolloutBudgetConfig {
            limit_tokens: 250,
            reminder_at_remaining_tokens: Vec::new(),
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
        };
        let budget = RolloutBudget::default();
        // A leftover per-run limit installed by a prior workflow run.
        budget.configure(workflow_output_weight_config(9_000));
        super::apply_workflow_budget(&budget, &serde_json::json!({}), Some(&baseline));
        assert_eq!(
            budget.limit(),
            Some(250),
            "an absent top-level run must return to the session baseline, not inherit 9000"
        );
    }

    /// With no session baseline, an absent top-level run resets to unmetered — the
    /// leftover per-run limit is cleared (finding #4).
    #[test]
    fn top_level_absent_budget_without_baseline_resets_to_unmetered() {
        use crate::rollout_budget::RolloutBudget;
        use crate::rollout_budget::workflow_output_weight_config;

        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(9_000));
        super::apply_workflow_budget(&budget, &serde_json::json!({}), None);
        assert_eq!(
            budget.limit(),
            None,
            "no baseline: an absent run resets to unmetered and clears the leftover limit"
        );
    }

    /// An explicit run budget is installed on top of the baseline (finding #2/#3): a
    /// declared `total` takes effect, and `total == 0` is a real zero ceiling.
    #[test]
    fn top_level_explicit_budget_takes_effect() {
        use crate::config::RolloutBudgetConfig;
        use crate::rollout_budget::RolloutBudget;

        let baseline = RolloutBudgetConfig {
            limit_tokens: 250,
            reminder_at_remaining_tokens: Vec::new(),
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
        };
        let budget = RolloutBudget::default();
        super::apply_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 100 } }),
            Some(&baseline),
        );
        assert_eq!(
            budget.limit(),
            Some(100),
            "an explicit run budget takes effect"
        );

        let budget = RolloutBudget::default();
        super::apply_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 0 } }),
            None,
        );
        assert_eq!(
            budget.limit(),
            Some(0),
            "an explicit zero is a real ceiling"
        );
    }

    /// Finding #7: a nested run that only `Yielded` is NOT treated as a completed
    /// result — it maps to `Rejected` (a clear JS throw) rather than resolving the
    /// `workflow()` promise with partial/null output.
    #[test]
    fn workflow_result_outcome_yielded_is_rejected_not_completed() {
        let response = RuntimeResponse::Yielded {
            cell_id: CellId::new("2".to_string()),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "partial".to_string(),
            }],
        };
        assert!(matches!(
            workflow_result_outcome(response),
            AgentSpawnOutcome::Rejected(_)
        ));
    }

    /// Finding #5: a nested child with no budget INHERITS the parent ceiling (the
    /// shared cell is left untouched, never reset).
    #[test]
    fn nested_budget_absent_inherits_parent_ceiling() {
        use crate::rollout_budget::RolloutBudget;
        use crate::rollout_budget::workflow_output_weight_config;
        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        let parent = budget.snapshot_config();
        super::configure_nested_workflow_budget(&budget, &serde_json::json!({}), parent.as_ref());
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "an absent nested budget must inherit the parent ceiling, not reset it"
        );
    }

    /// Finding #5: a nested child cannot RAISE the parent ceiling — an over-large
    /// child limit is clamped to the parent's; a tighter child limit is honored.
    #[test]
    fn nested_budget_clamps_child_to_parent_ceiling() {
        use crate::rollout_budget::RolloutBudget;
        use crate::rollout_budget::workflow_output_weight_config;

        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        let parent = budget.snapshot_config();
        // Child asks for 5_000 under a 1_000 parent: clamped to 1_000.
        super::configure_nested_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 5_000 } }),
            parent.as_ref(),
        );
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "child must not raise the ceiling"
        );

        // Restore and try a tighter child limit: honored.
        budget.restore_config(parent.clone());
        super::configure_nested_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 250 } }),
            parent.as_ref(),
        );
        assert_eq!(
            budget.limit(),
            Some(250),
            "a tighter child limit is honored"
        );
    }

    /// UAT-10 budget lifecycle across nesting: the parent ceiling is restored after EACH nested
    /// child (sequential "concurrent children" safety), an ABSENT child inherits the live parent
    /// ceiling, an explicit child ceiling only TIGHTENS, and the tree-wide spend is preserved
    /// across every nested run (a child can never evade the parent ceiling by nesting).
    ///
    /// This drives the exact `run_workflow_by_name` budget sequence — `snapshot_config` →
    /// `configure_nested_workflow_budget` → child `record_usage` → `restore_config` — against a bare
    /// `RolloutBudget`, so the invariants the integration lane cannot read directly (shared spend,
    /// restored ceiling) are asserted deterministically.
    #[test]
    fn nested_budget_lifecycle_preserves_shared_spend_and_restores_parent_ceiling() {
        use crate::rollout_budget::RolloutBudget;
        use crate::rollout_budget::workflow_output_weight_config;
        use codex_protocol::protocol::TokenUsage;

        fn output_usage(output_tokens: i64) -> TokenUsage {
            TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens,
                reasoning_output_tokens: 0,
                total_tokens: output_tokens,
            }
        }

        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        budget.record_usage(&output_usage(100)); // parent turn spend

        // Child A: explicit tighter ceiling (250 < 1000). Mirrors one nested run.
        let snapshot_a = budget.snapshot_config();
        super::configure_nested_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 250 } }),
            snapshot_a.as_ref(),
        );
        assert_eq!(budget.limit(), Some(250), "child A tightens the ceiling");
        budget.record_usage(&output_usage(80)); // child A subagent spend charged to the shared tree
        budget.restore_config(snapshot_a);
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "the parent ceiling is restored after child A"
        );
        assert_eq!(
            budget.spent(),
            180,
            "child A spend stays charged against the shared tree-wide budget after restore"
        );

        // Child B: ABSENT budget — inherits the live parent ceiling (never resets it) and keeps
        // spending against the shared counter.
        let snapshot_b = budget.snapshot_config();
        super::configure_nested_workflow_budget(
            &budget,
            &serde_json::json!({}),
            snapshot_b.as_ref(),
        );
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "an absent nested child inherits the parent ceiling"
        );
        budget.record_usage(&output_usage(20));
        budget.restore_config(snapshot_b);
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "the parent ceiling survives an absent-budget child"
        );
        assert_eq!(
            budget.spent(),
            200,
            "parent + child A + child B spend all accumulate on the shared budget"
        );

        // Child C: an over-large ceiling can never RAISE the tree-wide budget.
        let snapshot_c = budget.snapshot_config();
        super::configure_nested_workflow_budget(
            &budget,
            &serde_json::json!({ "budget": { "total": 100_000 } }),
            snapshot_c.as_ref(),
        );
        assert_eq!(
            budget.limit(),
            Some(1_000),
            "a child cannot raise the ceiling by nesting — it is clamped to the parent's"
        );
        budget.restore_config(snapshot_c);
        assert_eq!(budget.limit(), Some(1_000));
        assert_eq!(
            budget.spent(),
            200,
            "shared spend is unchanged by the clamped child C"
        );
    }

    /// Finding #8: the execution read is hard-bounded — a file over the cap is
    /// rejected with an error instead of being materialized in full.
    #[tokio::test]
    async fn bounded_workflow_read_rejects_oversized_file() {
        use super::MAX_WORKFLOW_SOURCE_BYTES;
        use super::read_workflow_source_bounded;

        let dir = tempfile::tempdir().expect("tempdir");
        let small = dir.path().join("small.js");
        tokio::fs::write(&small, b"export const meta = {};")
            .await
            .expect("write small");
        assert!(read_workflow_source_bounded(&small).await.is_ok());

        let big = dir.path().join("big.js");
        let body = vec![b'x'; (MAX_WORKFLOW_SOURCE_BYTES as usize) + 1];
        tokio::fs::write(&big, &body).await.expect("write big");
        let err = read_workflow_source_bounded(&big)
            .await
            .expect_err("oversized file must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn short_error_is_unchanged() {
        let message = "invalid workflow `meta` manifest: boom".to_string();
        assert_eq!(truncate_model_error(message.clone()), message);
    }

    #[test]
    fn oversized_error_is_hard_truncated_with_marker() {
        let message = "z".repeat(MAX_MODEL_ERROR_BYTES * 4);
        let truncated = truncate_model_error(message);
        assert!(
            truncated.ends_with(ERROR_TRUNCATION_MARKER),
            "expected truncation marker, got: {truncated}"
        );
        // The marker length is reserved inside the budget, so the final string —
        // prefix plus marker — never exceeds the hard cap.
        assert!(
            truncated.len() <= MAX_MODEL_ERROR_BYTES,
            "truncated error is {} bytes, over the {MAX_MODEL_ERROR_BYTES}-byte cap",
            truncated.len()
        );
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // A multi-byte char straddling the cap must not panic or split a code
        // point: build a string of 3-byte chars whose boundary is not aligned to
        // the byte cap.
        let message = "€".repeat(MAX_MODEL_ERROR_BYTES);
        let truncated = truncate_model_error(message);
        // Round-trips as valid UTF-8 (implicitly, since it is a `String`) and is
        // bounded by the hard cap (marker included).
        assert!(truncated.len() <= MAX_MODEL_ERROR_BYTES);
        assert!(truncated.ends_with(ERROR_TRUNCATION_MARKER));
    }

    #[test]
    fn bound_runtime_error_caps_result_error_text() {
        // Script failures arrive on the `Ok` path as `RuntimeResponse::Result`
        // carrying an unbounded `error_text`. Bounding must cap it at the same
        // hard limit so a workflow cannot smuggle a huge error to the model.
        let huge = "q".repeat(MAX_MODEL_ERROR_BYTES * 8);
        let mut response = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: Some(huge),
        };
        bound_runtime_error(&mut response);
        match response {
            RuntimeResponse::Result { error_text, .. } => {
                let text = error_text.expect("error text preserved");
                assert!(
                    text.len() <= MAX_MODEL_ERROR_BYTES,
                    "model-visible error text is {} bytes, over the cap",
                    text.len()
                );
                assert!(text.ends_with(ERROR_TRUNCATION_MARKER));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn bound_runtime_error_leaves_short_error_and_success_untouched() {
        let short = "boom".to_string();
        let mut response = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: Some(short.clone()),
        };
        bound_runtime_error(&mut response);
        match &response {
            RuntimeResponse::Result { error_text, .. } => {
                assert_eq!(error_text.as_deref(), Some(short.as_str()));
            }
            other => panic!("expected Result, got {other:?}"),
        }

        // A successful result (no error text) is left as-is.
        let mut ok = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: Vec::new(),
            error_text: None,
        };
        bound_runtime_error(&mut ok);
        match ok {
            RuntimeResponse::Result { error_text, .. } => assert!(error_text.is_none()),
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn bound_model_error_preserves_variant_and_truncates() {
        let long = "y".repeat(MAX_MODEL_ERROR_BYTES * 2);
        match bound_model_error(FunctionCallError::RespondToModel(long.clone())) {
            FunctionCallError::RespondToModel(message) => {
                assert!(message.ends_with("… [error truncated]"));
                assert!(message.len() < long.len());
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
        match bound_model_error(FunctionCallError::Fatal(long)) {
            FunctionCallError::Fatal(message) => {
                assert!(message.ends_with("… [error truncated]"));
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }
}
