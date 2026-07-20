use std::collections::HashMap;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_protocol::ToolName;
use codex_protocol::protocol::WorkflowEvent;
use codex_workflow_journal::AgentCallLine;
use serde_json::Value as JsonValue;

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    TimeoutFired { id: u64 },
    ObservePendingFrontier,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PendingRuntimeMode {
    #[cfg(test)]
    Continue,
    PauseUntilResumed,
}

#[derive(Debug)]
pub(crate) enum RuntimeControlCommand {
    Continue,
    Resume,
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    Pending,
    ContentItem(FunctionCallOutputContentItem),
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
    },
    Notify {
        call_id: String,
        text: String,
    },
    /// A workflow `agent(prompt, opts?)` spawn request (§3 async bridge op; §7
    /// invocation ordinal). Structurally mirrors [`RuntimeEvent::ToolCall`]: the
    /// `agent_callback` mints a resolver stored in `pending_tool_calls` under
    /// `id`, stamps `ordinal` synchronously from `RuntimeState.next_agent_ordinal`
    /// (source-ordered even under `Promise.all`), and emits this event for the
    /// cell actor to route to the spawn helper. Emitted only for workflow runs.
    /// This is the pure type surface both `P1-agent-callback` (emits) and
    /// `P1-cellactor-spawn-dispatch` (consumes) build against; no code
    /// constructs it yet.
    #[allow(
        dead_code,
        reason = "constructed by the later agent_callback / cell_actor dispatch tickets"
    )]
    AgentCall {
        id: String,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        ordinal: u64,
        prompt: String,
        opts: AgentCallOpts,
    },
    /// A prefix-replay cache hit for a resumed run (§7 resume algorithm step 3).
    /// `agent_callback` stamped the same source-order `ordinal` as a live call,
    /// but the recomputed `(prompt, opts)` key matched the journaled entry and
    /// its `status` was `completed`, so the promise is served from the journaled
    /// `return` WITHOUT spawning a subagent. The isolate keeps a resolver in
    /// `pending_tool_calls` under `id`; the cell actor re-appends `entry` to the
    /// NEW run's journal and charges `entry.tokens_spent` to the run-local meter
    /// (`CellHost::replay_agent`) so `spent()`/`remaining()` and the ceiling throw
    /// track the original run. A successful host acknowledgement resolves the
    /// promise via `RuntimeCommand::ToolResponse`; a failed acknowledgement rejects
    /// it via `RuntimeCommand::ToolError`, so unaccounted cached output is never returned.
    /// Emitted only for workflow runs, and only while replay is active.
    AgentReplay {
        id: String,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        entry: Box<AgentCallLine>,
    },
    /// A workflow `phase(title)` narrator/grouping marker. Emitted only for
    /// workflow runs; the protocol `WorkflowPhaseBegin/End` mapping + journaling
    /// that read `title` are later tickets, so the field is not yet consumed by
    /// non-test code.
    Phase {
        #[allow(dead_code, reason = "consumed by the later protocol/journal tickets")]
        title: String,
    },
    /// A workflow `log(msg)` narrator line. Thin alias over the `Notify` path
    /// (same text plumbing, distinct event). Emitted only for workflow runs; the
    /// protocol `WorkflowLog` mapping + journaling that read `message` are later
    /// tickets, so the field is not yet consumed by non-test code.
    WorkflowLog {
        #[allow(dead_code, reason = "consumed by the later protocol/journal tickets")]
        message: String,
    },
    /// A workflow `workflow(nameOrRef, args)` nested-run request (§4 `workflow()`;
    /// §3 async bridge op). Structurally mirrors [`RuntimeEvent::AgentCall`]: the
    /// `workflow_callback` mints a resolver stored in `pending_tool_calls` under
    /// `id`, stamps `id` synchronously from `RuntimeState.next_workflow_call_id`,
    /// and emits this event for the cell actor to route to the nested-run host
    /// handler. Emitted only for workflow runs. This is the pure bridge half that
    /// `P2-workflow-global-callback` emits; the host handler that consumes it
    /// (registry load + nested re-enter) is `P2-workflow-registry-reenter`, so no
    /// non-test code reads `name`/`args` yet.
    WorkflowCall {
        #[allow(
            dead_code,
            reason = "consumed by the later workflow host handler ticket (P2-workflow-registry-reenter)"
        )]
        id: String,
        #[allow(
            dead_code,
            reason = "consumed by the later workflow host handler ticket (P2-workflow-registry-reenter)"
        )]
        name: String,
        #[allow(
            dead_code,
            reason = "consumed by the later workflow host handler ticket (P2-workflow-registry-reenter)"
        )]
        args: Option<JsonValue>,
    },
    /// Source-ordered public workflow progress authored inside the deterministic isolate and
    /// forwarded through the host delegate to core.
    WorkflowProgress(Box<WorkflowEvent>),
    Result {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
    ThreadPanicked,
}
