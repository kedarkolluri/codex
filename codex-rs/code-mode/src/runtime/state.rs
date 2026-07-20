use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::WorkflowBudgetHandle;
use codex_workflow_journal::AgentCallLine;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use super::RuntimeCommand;
use super::RuntimeEvent;
use super::timers;

/// Prefix-replay scaffolding for a resumed workflow run (§7 "Resume algorithm").
///
/// On `resumeFromRunId`, the prior run's `journal.jsonl` is loaded tail-first
/// into `entries[0..M]` (the `agent_call` lines, keyed by their invocation
/// ordinal) and this state is seeded [`ReplayState::seed`] with `active = true`.
/// During the deterministic prefix, `agent_callback` (via `P3-resume-prefix-loop`)
/// consults [`ReplayState::entry`] at each issued ordinal: on a matching
/// `(prompt, opts)` key it resolves the promise from the journaled `return` and
/// re-adds the journaled `tokens_spent` through [`ReplayState::add_replay_spent`]
/// so `spent()`/`remaining()` and the ceiling throw land at the identical
/// ordinal. At the first divergence (missing entry, key mismatch, or a
/// non-`completed` status) it calls [`ReplayState::disable`], after which replay
/// is off **permanently** for the run — there is deliberately no re-enable path,
/// which is the "first divergence goes live and never returns to replay"
/// guarantee. A fresh (non-resume) run uses [`ReplayState::fresh`]: no entries,
/// `active = false`, so it never diverts from live dispatch.
///
/// This ticket (`P3-runtime-replay-state`) provides only the state + init +
/// accessors; the replay decision logic that drives them is `P3-resume-prefix-loop`.
#[derive(Debug, Default)]
pub(super) struct ReplayState {
    /// Prior journal `agent_call` entries indexed by their invocation ordinal
    /// (§7 "invocation ordinal / cache key"). Empty for a fresh run.
    entries: HashMap<u64, AgentCallLine>,
    /// Prefix length `M` — the count of journaled `agent_call` entries the
    /// resume loop may replay before it must go live. `0` for a fresh run.
    prefix_len: u64,
    /// Replay-only budget accumulator: the sum of journaled `tokens_spent`
    /// re-added while serving the prefix from cache (§7 resume step 3 /
    /// §8 "Resume determinism of budget"), so the resumed run's spend curve is
    /// byte-identical to the original's. Never advanced on a fresh run.
    replay_spent: i64,
    /// `true` only while the run is still inside the unchanged prefix. Seeded
    /// `true` on resume, latched `false` at the first divergence and never
    /// re-enabled; always `false` for a fresh run.
    active: bool,
}

impl ReplayState {
    /// State for a fresh (non-resume) run: no entries, replay inactive. Fan-out
    /// behaves exactly as before — `active = false` means the resume loop never
    /// diverts from live dispatch.
    pub(super) fn fresh() -> Self {
        Self::default()
    }

    /// Seed from a loaded prior journal: index the `agent_call` `entries` by
    /// their invocation ordinal, set the prefix length `M` to the number of
    /// entries, and arm replay (`active = true`). The replay budget accumulator
    /// starts at `0` and is grown per replayed entry via
    /// [`ReplayState::add_replay_spent`].
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "seed path is driven by the later P3-resume-entry ticket"
        )
    )]
    pub(super) fn seed(entries: Vec<AgentCallLine>) -> Self {
        let prefix_len = entries.len() as u64;
        let entries = entries
            .into_iter()
            .map(|entry| (entry.ordinal, entry))
            .collect();
        Self {
            entries,
            prefix_len,
            replay_spent: 0,
            active: true,
        }
    }

    /// Whether prefix-replay is still active. Once [`disable`](Self::disable)
    /// has latched this `false`, it stays `false` for the rest of the run.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the later P3-resume-prefix-loop ticket"
        )
    )]
    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    /// Prefix length `M` (the count of replayable journaled `agent_call`
    /// entries). The resume loop replays only ordinals `i < M`.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the later P3-resume-prefix-loop ticket"
        )
    )]
    pub(super) fn prefix_len(&self) -> u64 {
        self.prefix_len
    }

    /// The journaled `agent_call` entry at `ordinal`, if one was recorded. The
    /// resume loop matches its `key` against the freshly computed `(prompt,
    /// opts)` key to decide replay-vs-divergence.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the later P3-resume-prefix-loop ticket"
        )
    )]
    pub(super) fn entry(&self, ordinal: u64) -> Option<&AgentCallLine> {
        self.entries.get(&ordinal)
    }

    /// Running total of journaled `tokens_spent` re-added during prefix replay.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the later P3-resume-prefix-loop ticket"
        )
    )]
    pub(super) fn replay_spent(&self) -> i64 {
        self.replay_spent
    }

    /// Re-add a replayed entry's journaled `tokens_spent` to the replay-only
    /// accumulator (§7 resume step 3). Saturating so a corrupt journal can never
    /// panic the run. No-op once replay has been disabled — divergent (live)
    /// calls meter through the real budget, not this accumulator.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "driven by the later P3-resume-prefix-loop ticket")
    )]
    pub(super) fn add_replay_spent(&mut self, tokens: i64) {
        if self.active {
            self.replay_spent = self.replay_spent.saturating_add(tokens);
        }
    }

    /// Latch replay off at the first divergence. Idempotent and one-way: there
    /// is no path back to `active = true` within a run, which is the "first
    /// changed/new call and everything after runs live" guarantee (§7).
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "driven by the later P3-resume-prefix-loop ticket")
    )]
    pub(super) fn disable(&mut self) {
        self.active = false;
    }
}

pub(super) struct RuntimeState {
    pub(super) event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pub(super) pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    pub(super) stored_values: HashMap<String, JsonValue>,
    pub(super) stored_value_writes: HashMap<String, JsonValue>,
    pub(super) enabled_tools: Vec<EnabledToolMetadata>,
    pub(super) next_tool_call_id: u64,
    pub(super) next_timeout_id: u64,
    /// Monotonic source-order counter for `agent()` invocations. Stamped onto
    /// each [`RuntimeEvent::AgentCall`] as its `ordinal` (§7 invocation ordinal),
    /// bumped synchronously in `agent_callback` before the promise returns so the
    /// sequence is deterministic across `parallel`/`Promise.all` concurrency and
    /// stable for prefix-replay cache keying. Initialized to `0`. Not yet read by
    /// non-test code; the `agent_callback` ticket wires the bump.
    #[allow(
        dead_code,
        reason = "stamped by the later agent_callback ticket (P1-agent-callback)"
    )]
    pub(super) next_agent_ordinal: u64,
    /// Topology IDs for groups and agent leaves share this source-order counter. It is deliberately
    /// independent from `next_agent_ordinal`, which remains the replay cache spine.
    pub(super) next_workflow_node_id: u64,
    /// Parent group active while a group-owned thunk or pipeline stage is invoked synchronously.
    pub(super) active_workflow_parent_node_id: Option<u64>,
    /// Explicit phase currently active in this run. Runs without a `phase()` call use the reducer's
    /// implicit root phase and leave this empty.
    pub(super) active_workflow_phase: Option<(u64, String)>,
    pub(super) next_workflow_phase_index: u64,
    pub(super) workflow_log_count: u64,
    pub(super) workflow_phase_count: u64,
    pub(super) workflow_output_bounds: codex_code_mode_protocol::WorkflowOutputBounds,
    /// Active groups and agent leaves. Phase transitions are rejected until this set is empty, so
    /// the public reducer never observes a phase end with live topology beneath it.
    pub(super) active_workflow_nodes: HashSet<u64>,
    /// Active `parallel()`/`pipeline()` nodes. Module completion waits for these orchestration
    /// barriers, while a bare unawaited root `agent()` is cancelled by normal cell teardown.
    pub(super) active_workflow_groups: HashSet<u64>,
    pub(super) pending_workflow_agent_nodes: HashMap<String, u64>,
    /// Promise-reaction context stack used by [`workflow_promise_hook`] to preserve group ancestry
    /// across arbitrary `await` boundaries and overlapping async chains.
    pub(super) workflow_parent_context_stack: Vec<Option<u64>>,
    /// Monotonic counter for `workflow(nameOrRef, args)` nested-run invocations.
    /// Stamped into each [`RuntimeEvent::WorkflowCall`]'s `id` (as
    /// `workflow-{n}`), bumped synchronously in `workflow_callback` before the
    /// promise returns so the id is unique and the resolver is retrievable from
    /// `pending_tool_calls` out-of-order. Initialized to `0`. Only advanced on
    /// workflow runs (the `workflow` global is gated behind `RuntimeState.workflow`).
    pub(super) next_workflow_call_id: u64,
    pub(super) tool_call_id: String,
    pub(super) runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    pub(super) exit_requested: bool,
    /// True when this cell is running a workflow script. Set from the explicit
    /// `ExecuteRequest::workflow` invocation mode (never inferred from source),
    /// so it is `true` only for the workflow handler path. Gates the workflow
    /// narrator globals (`phase`/`log`) so they never leak into plain code-mode
    /// exec sessions.
    pub(super) workflow: bool,
    /// Invocation JSON injected read-only as the `args` global. Only populated
    /// (and only installed) for workflow runs; see
    /// [`codex_code_mode_protocol::ExecuteRequest::args`].
    pub(super) args: Option<JsonValue>,
    /// Host-minted uuid v7 run identifier exposed read-only as `workflow.runId`.
    /// Only populated (and only installed) for workflow runs; see
    /// [`codex_code_mode_protocol::ExecuteRequest::run_id`].
    pub(super) run_id: Option<String>,
    /// Runtime-owned budget mirror backing the `budget` global's `spent()` /
    /// `remaining()` native functions. Callback refreshes are visible immediately
    /// when a workflow re-reads them after awaiting a subagent.
    pub(super) budget: Option<Arc<dyn WorkflowBudgetHandle>>,
    /// Prefix-replay scaffolding for a resumed run (§7). Seeded with the prior
    /// journal's `agent_call` entries and `active = true` on resume; left in the
    /// [`ReplayState::fresh`] (empty, inactive) shape for a fresh run so fan-out
    /// behaviour is unchanged. Driven by the later `P3-resume-prefix-loop` ticket
    /// through the [`ReplayState`] accessors; unread by non-test code today, like
    /// `next_agent_ordinal`.
    pub(super) replay: ReplayState,
}

impl RuntimeState {
    /// Shared read view of the prefix-replay scaffolding (§7). The resume loop
    /// (`P3-resume-prefix-loop`) consults this at each issued `agent()` ordinal.
    #[allow(
        dead_code,
        reason = "consumed by the later P3-resume-prefix-loop ticket"
    )]
    pub(super) fn replay(&self) -> &ReplayState {
        &self.replay
    }

    /// Mutable view of the prefix-replay scaffolding, for the resume loop to
    /// re-add replayed `tokens_spent` and latch replay off at first divergence.
    #[allow(dead_code, reason = "driven by the later P3-resume-prefix-loop ticket")]
    pub(super) fn replay_mut(&mut self) -> &mut ReplayState {
        &mut self.replay
    }
}

/// Unit coverage for the [`ReplayState`] prefix-replay scaffolding
/// (`P3-runtime-replay-state`). These exercise the state + init + accessors in
/// isolation — the resume decision logic that drives them is
/// `P3-resume-prefix-loop`.
#[cfg(test)]
#[path = "replay_state_tests.rs"]
mod tests;
