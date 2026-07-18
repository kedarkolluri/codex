use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::AgentCallOpts;
use codex_code_mode::AgentSpawnFuture;
use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::call_nested_tool;
use super::scheduler::AgentCapReached;
use super::scheduler::SpawnAttempt;
use super::scheduler::WorkflowScheduler;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::control::spawn_await::workflow_agent_nickname_preference;
use crate::agent::control::spawn_await_opts::SpawnAgentConfigOverrides;
use crate::session::step_context::StepContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    dispatch_gates: Arc<Mutex<HashMap<CellId, watch::Sender<bool>>>>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new() -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            dispatch_tx,
            dispatch_rx,
            dispatch_gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        dispatch_gate(&self.dispatch_gates, cell_id).send_replace(true);
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        remove_dispatch_gate(&self.dispatch_gates, cell_id);
    }

    pub(super) fn start_turn_worker(
        &self,
        exec: ExecContext,
        router: Arc<ToolRouter>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> CodeModeDispatchWorker {
        let tool_runtime =
            ToolCallRuntime::new(router, Arc::clone(&exec.session), step_context, tracker);
        // One `WorkflowScheduler` per turn worker (i.e. per workflow run): every `agent()` call in
        // the run dispatches to this single `CoreTurnHost`, so its scheduler is what bounds the
        // whole `parallel()`/`pipeline()` fan-out's concurrency and enforces the per-run lifetime
        // cap (spec §5). Cap = `min(16, cores-2)` clamped by the parent turn's
        // `effective_agent_max_threads` (raised to the workflow ceiling).
        let scheduler = WorkflowScheduler::new(
            exec.turn
                .config
                .effective_agent_max_threads(exec.turn.multi_agent_version),
        );
        let host = Arc::new(CoreTurnHost {
            exec,
            tool_runtime,
            scheduler,
        });
        let dispatch_rx = self.dispatch_rx.clone();
        let dispatch_gates = Arc::clone(&self.dispatch_gates);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    message = dispatch_rx.recv() => message.ok(),
                };
                let Some(message) = message else {
                    break;
                };
                match message {
                    DispatchMessage::Notify {
                        call_id,
                        cell_id,
                        text,
                        cancellation_token,
                        response_tx,
                    } => {
                        let response = if wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            host.notify(call_id, cell_id, text).await
                        } else {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            Err("code mode notification cancelled".to_string())
                        };
                        let _ = response_tx.send(response);
                    }
                    DispatchMessage::InvokeTool {
                        invocation,
                        cancellation_token,
                        response_tx,
                    } => {
                        let cell_id = invocation.cell_id.clone();
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            let response = tokio::select! {
                                response = host.invoke_tool(
                                    invocation,
                                    cancellation_token.clone(),
                                ) => response,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let _ = response_tx.send(response);
                        });
                    }
                    DispatchMessage::SpawnAgent {
                        cell_id,
                        prompt,
                        ordinal,
                        opts,
                        cancellation_token,
                        response_tx,
                    } => {
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        // One independent task per `agent()` call: N concurrent calls run through the
                        // spawn helper concurrently and resolve out-of-order, with nothing in the
                        // dispatch loop serializing them (the loop only enqueues).
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            let result = tokio::select! {
                                result = host.spawn_agent(prompt, ordinal, opts) => result,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let _ = response_tx.send(result);
                        });
                    }
                }
            }
        });
        CodeModeDispatchWorker {
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

fn dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
) -> watch::Sender<bool> {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates
        .entry(cell_id.clone())
        .or_insert_with(|| watch::channel(false).0)
        .clone()
}

fn remove_dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
) {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates.remove(cell_id);
}

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let mut ready_rx = dispatch_gate(dispatch_gates, cell_id).subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn spawn_agent<'a>(
        &'a self,
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            // A cancelled call or an unavailable/stopped dispatcher resolves the isolate promise to
            // JS `null` (death-is-null) rather than throwing — only an admission-time cap rejection
            // (surfaced by the host as `AgentSpawnOutcome::Rejected`) throws.
            if cancellation_token.is_cancelled() {
                return AgentSpawnOutcome::Failed;
            }
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .dispatch_tx
                .send(DispatchMessage::SpawnAgent {
                    cell_id,
                    prompt,
                    ordinal,
                    opts,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .is_err()
            {
                return AgentSpawnOutcome::Failed;
            }
            tokio::select! {
                result = response_rx => result.unwrap_or(AgentSpawnOutcome::Failed),
                _ = cancellation_token.cancelled() => AgentSpawnOutcome::Failed,
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.close_cell(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    SpawnAgent {
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
        // Three-way [`AgentSpawnOutcome`] (SEAM CONTRACT): `Completed(value)` on success (a JSON
        // string when schemaless, or the validated `opts.schema` object), `Failed` on agent
        // death/abort/parse-fail (JS null), and `Rejected(msg)` when a scheduler admission cap or a
        // bounds check refuses the spawn (a JS throw once the seam lands).
        response_tx: oneshot::Sender<AgentSpawnOutcome>,
    },
}

pub(crate) struct CodeModeDispatchWorker {
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

/// Hard byte ceiling on the incoming `agent()` prompt before it becomes child context. A workflow
/// body can build an arbitrarily large prompt string in JS; without a bound it would flow verbatim
/// into the child's first-turn `UserInput`, blowing the per-agent ~10K-token individual-context
/// budget (spec §6). 64 KiB (~16K tokens) is a generous ceiling above which the prompt is truncated
/// with a marker rather than failing the call.
const WORKFLOW_PROMPT_MAX_BYTES: usize = 64 * 1024;

/// Hard ceiling on the serialized byte size of an `agent()` `opts.schema`. The schema is copied into
/// every child prompt and recompiled on each return; an unbounded one is both a context-budget and a
/// CPU DoS. 32 KiB is well above any legitimate structured-output schema.
const WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES: usize = 32 * 1024;

/// Hard ceiling on `opts.schema` nesting depth. `serde_json` already caps deserialization recursion,
/// but the schema is re-walked/compiled on every return, so an adversarially deep schema is bounded
/// here before use. 64 levels is far beyond any real JSON Schema.
const WORKFLOW_SCHEMA_MAX_DEPTH: usize = 64;

struct CoreTurnHost {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    /// Per-run concurrency + lifetime scheduler shared by every `agent()` call in this workflow run
    /// (spec §5). Constructed once in `start_turn_worker`; `admit` bounds concurrent spawns and
    /// enforces the monotonic lifetime cap.
    scheduler: WorkflowScheduler,
}

impl CoreTurnHost {
    async fn invoke_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        call_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation_token,
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Route a workflow `agent(prompt, opts?)` call through the per-run [`WorkflowScheduler`] into
    /// the wave-2 spawn keystone
    /// [`crate::agent::control::AgentControl::spawn_and_await_final_message`], resolving to an
    /// [`AgentSpawnOutcome`].
    ///
    /// ## Admission (spec §5) & determinism
    ///
    /// Before spawning, the incoming prompt is byte-capped ([`cap_prompt_bytes`]) and any
    /// `opts.schema` is bounded ([`ensure_schema_within_bounds`]); an over-limit schema returns
    /// [`AgentSpawnOutcome::Rejected`] without consuming a lifetime slot. The call is then admitted
    /// through [`WorkflowScheduler::admit`]: the monotonic lifetime CAS runs first (over-cap ->
    /// `Rejected("AgentCapReached")`, no permit awaited), then a concurrency permit is held across
    /// the spawn and released on finalize on every path. The child nickname is derived purely from
    /// the invocation `ordinal` via [`workflow_agent_nickname_preference`] (no `rand`).
    ///
    /// The keystone constructs the `Subagent` source itself and drives the child's first turn to
    /// completion over the non-competing event tap. A normal final message becomes
    /// [`AgentSpawnOutcome::Completed`]; a dead/aborted child (or a config-build/spawn/submit
    /// failure, or a schema parse/validation failure) becomes [`AgentSpawnOutcome::Failed`] (JS
    /// null). `opts.model` / `opts.effort` / `opts.agentType` are threaded as
    /// [`SpawnAgentConfigOverrides`] and applied to the inherited child config before the spawn;
    /// omitted overrides inherit the parent turn.
    ///
    /// ## Structured output (`opts.schema`, spec §6)
    ///
    /// When `opts.schema` is present it is threaded onto the child's first turn as
    /// `final_output_json_schema`, forcing a StructuredOutput (`output_schema_strict = true`) final
    /// message. On return the raw final text is `serde_json`-parsed and, as **defense-in-depth**
    /// (engine strict mode is enforced for OpenAI providers but not guaranteed for all), re-validated
    /// against the JSON Schema with the `jsonschema` crate before the parsed object is resolved back
    /// to JS. A parse or validation failure resolves to `None` (JS `null`) per the death-is-null
    /// contract. Without `opts.schema` the plain final text is resolved as a JSON string. See
    /// [`finalize_agent_output`].
    async fn spawn_agent(
        &self,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
    ) -> AgentSpawnOutcome {
        // Bound the incoming `opts.schema` BEFORE it is threaded into any child prompt or recompiled
        // on return: an over-large / over-deep schema is a caller error, so REJECT the call with a
        // clear reason rather than admitting it and paying the context/CPU cost per child.
        if let Some(schema) = opts.schema.as_ref()
            && let Err(reason) = ensure_schema_within_bounds(schema)
        {
            warn!("workflow agent() rejected: {reason}");
            return AgentSpawnOutcome::Rejected(reason);
        }
        // Bound the incoming prompt before it becomes child context (truncate with a marker so an
        // oversized prompt degrades gracefully rather than failing the whole call).
        let prompt = cap_prompt_bytes(prompt);

        // TODO(M2, spec §5 admission-order step 1): budget pre-admission hook goes here, ahead of
        // the lifetime CAS + concurrency permit below. M2 owns budget; do not implement it here.

        let session = &self.exec.session;
        let turn = self.exec.turn.as_ref();
        let scheduler = &self.scheduler;
        let schema = opts.schema;
        let overrides = SpawnAgentConfigOverrides {
            model: opts.model,
            effort: opts.effort,
            agent_type: opts.agent_type,
        };
        // Nickname is a pure function of the invocation ordinal (spec §7, no `rand`): the registry's
        // preferred-name branch reserves it verbatim (deterministically resolving any collision).
        let preferred_agent_nickname = Some(workflow_agent_nickname_preference(ordinal as usize));

        // Admit through the shared per-run scheduler (spec §5 admission order): the lifetime CAS
        // (step 2) runs first and rejects over-cap calls with `AgentCapReached` WITHOUT awaiting a
        // permit; then a concurrency permit is acquired (step 4) before the child is spawned, and
        // dropped on finalize on every path (step 6) via the permit RAII guard inside `admit`.
        let admit_result = scheduler
            .admit(|| {
                // Cloned per admission attempt (the scheduler may re-invoke on a registry-backstop
                // requeue); `session`/`turn`/`base_instructions` are cheap shared references.
                let prompt = prompt.clone();
                let schema = schema.clone();
                let overrides = overrides.clone();
                let preferred_agent_nickname = preferred_agent_nickname.clone();
                async move {
                    let base_instructions = session.get_base_instructions().await;
                    let parent_thread_id = session.thread_id;
                    let options = SpawnAgentOptions {
                        parent_thread_id: Some(parent_thread_id),
                        environments: Some(turn.environments.to_selections()),
                        preferred_agent_nickname,
                        ..Default::default()
                    };
                    let final_text = session
                        .services
                        .agent_control
                        .spawn_and_await_final_message(
                            &base_instructions,
                            turn,
                            parent_thread_id,
                            vec![UserInput::Text {
                                text: prompt,
                                text_elements: Vec::new(),
                            }],
                            schema.clone(),
                            overrides,
                            options,
                        )
                        .await;
                    // A normal final message -> `Completed`; agent death/abort/schema parse-fail ->
                    // `Failed`. Both are terminal `Finalized` outcomes, so the permit releases either
                    // way (the registry-backstop `AgentLimitReached` requeue is a scheduler unit
                    // concern; the keystone maps a saturated-registry spawn error to `None` here).
                    let outcome = match finalize_agent_output(final_text, schema.as_ref()) {
                        Some(value) => AgentSpawnOutcome::Completed(value),
                        None => AgentSpawnOutcome::Failed,
                    };
                    SpawnAttempt::Finalized(outcome)
                }
            })
            .await;

        match admit_result {
            Ok(outcome) => outcome,
            // Lifetime cap reached (spec §5): terminal and monotonic — surfaced as a JS throw.
            Err(AgentCapReached { .. }) => {
                AgentSpawnOutcome::Rejected("AgentCapReached".to_string())
            }
        }
    }

    async fn notify(&self, call_id: String, cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_if_running(vec![ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .map_err(|_| {
                format!("failed to inject exec notify message for cell {cell_id}: no active turn")
            })
    }
}

/// Turn a workflow `agent()` child's raw final message into the JS value the isolate promise
/// resolves to (spec §6 structured output).
///
/// - `final_text == None` (a dead/aborted child, or a config-build/spawn/submit failure) → `None`
///   (JS `null`).
/// - `schema == None` (a schemaless call) → `Some(JsonValue::String(final_text))` (a plain JS
///   string), so an ordinary `agent()` still returns the assistant text.
/// - `schema == Some` (structured output) → `serde_json`-parse `final_text`, then, as
///   **defense-in-depth**, re-validate the parsed instance against the JSON Schema with the
///   `jsonschema` crate. Strict mode is engine-enforced for OpenAI providers but not guaranteed for
///   all, so this recheck always runs regardless of what the engine claims. A parse failure, an
///   uncompilable schema, or a non-conformant instance each resolve to `None` (JS `null`) per the
///   death-is-null contract — `agent()` never throws for agent failure.
fn finalize_agent_output(
    final_text: Option<String>,
    schema: Option<&JsonValue>,
) -> Option<JsonValue> {
    let final_text = final_text?;
    let Some(schema) = schema else {
        return Some(JsonValue::String(final_text));
    };
    let parsed: JsonValue = serde_json::from_str(&final_text)
        .map_err(|err| warn!("workflow agent() structured output is not valid JSON: {err}"))
        .ok()?;
    // Belt-and-suspenders: compile the schema and re-validate the parsed instance even though the
    // child was asked for strict mode. An uncompilable schema is treated as a validation failure
    // (null) rather than trusting unvalidated model output.
    let validator = jsonschema::validator_for(schema)
        .map_err(|err| warn!("workflow agent() opts.schema is not a valid JSON Schema: {err}"))
        .ok()?;
    if validator.is_valid(&parsed) {
        Some(parsed)
    } else {
        warn!("workflow agent() structured output failed JSON Schema validation");
        None
    }
}

/// Enforce the [`WORKFLOW_PROMPT_MAX_BYTES`] ceiling on an `agent()` prompt before it becomes child
/// context. Returns the prompt unchanged when within budget; otherwise truncates it on a UTF-8 char
/// boundary and appends a marker so the child (and any log) sees that truncation happened. Pure and
/// deterministic — no `Date`/`Math`/`rand`.
fn cap_prompt_bytes(prompt: String) -> String {
    if prompt.len() <= WORKFLOW_PROMPT_MAX_BYTES {
        return prompt;
    }
    // Largest char boundary at or below the cap, so we never split a multi-byte code point.
    let mut end = WORKFLOW_PROMPT_MAX_BYTES;
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = prompt[..end].to_string();
    truncated.push_str("\n[workflow agent() prompt truncated: exceeded 64 KiB]");
    warn!(
        "workflow agent() prompt exceeded {WORKFLOW_PROMPT_MAX_BYTES} bytes; truncated to fit the \
         child context budget"
    );
    truncated
}

/// Bound an `agent()` `opts.schema` BEFORE it is threaded into a child prompt or recompiled on
/// return: reject a schema whose serialized size exceeds [`WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES`] or
/// whose nesting exceeds [`WORKFLOW_SCHEMA_MAX_DEPTH`]. The size check runs first so the (cheap)
/// depth walk only ever runs on an already-small structure. Returns an actionable reason on refusal.
fn ensure_schema_within_bounds(schema: &JsonValue) -> Result<(), String> {
    let serialized_len = serde_json::to_vec(schema)
        .map_err(|err| format!("opts.schema could not be serialized: {err}"))?
        .len();
    if serialized_len > WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES {
        return Err(format!(
            "opts.schema is too large ({serialized_len} bytes > {WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES} byte cap)"
        ));
    }
    let depth = json_depth(schema, WORKFLOW_SCHEMA_MAX_DEPTH);
    if depth > WORKFLOW_SCHEMA_MAX_DEPTH {
        return Err(format!(
            "opts.schema nesting is too deep (exceeds the {WORKFLOW_SCHEMA_MAX_DEPTH}-level cap)"
        ));
    }
    Ok(())
}

/// Iterative (stack-safe) maximum nesting depth of a JSON value, short-circuiting once `limit` is
/// exceeded so an adversarial structure cannot force an unbounded walk. A scalar is depth 1.
fn json_depth(value: &JsonValue, limit: usize) -> usize {
    let mut max_depth = 0usize;
    let mut stack = vec![(value, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        max_depth = max_depth.max(depth);
        if depth > limit {
            // Already over the cap; no need to descend further.
            return max_depth;
        }
        match node {
            JsonValue::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            JsonValue::Object(map) => {
                for item in map.values() {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    max_depth
}

#[cfg(test)]
mod prompt_and_schema_bound_tests {
    use super::*;
    use serde_json::json;

    /// A prompt within the byte ceiling is returned verbatim (no marker).
    #[test]
    fn prompt_within_cap_is_unchanged() {
        let prompt = "a".repeat(1024);
        assert_eq!(cap_prompt_bytes(prompt.clone()), prompt);
    }

    /// A prompt over the ceiling is truncated to fit and carries the truncation marker.
    #[test]
    fn oversized_prompt_is_truncated_with_marker() {
        let prompt = "a".repeat(WORKFLOW_PROMPT_MAX_BYTES + 4096);
        let capped = cap_prompt_bytes(prompt);
        assert!(
            capped.len() <= WORKFLOW_PROMPT_MAX_BYTES + 64,
            "truncated prompt (plus marker) must be bounded, got {} bytes",
            capped.len()
        );
        assert!(
            capped.ends_with("truncated: exceeded 64 KiB]"),
            "a truncated prompt must carry the marker"
        );
    }

    /// Truncation never splits a multi-byte UTF-8 code point (the result is always valid UTF-8).
    #[test]
    fn oversized_multibyte_prompt_truncates_on_char_boundary() {
        // 'é' is 2 bytes; a run of them can only be split on an even boundary.
        let prompt = "é".repeat(WORKFLOW_PROMPT_MAX_BYTES);
        let capped = cap_prompt_bytes(prompt);
        // A round-trip through `str` would panic on a bad boundary; reaching here proves validity.
        assert!(capped.contains("truncated"));
    }

    /// A small, shallow schema passes the bounds check.
    #[test]
    fn small_schema_is_within_bounds() {
        let schema = json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
        });
        assert!(ensure_schema_within_bounds(&schema).is_ok());
    }

    /// A schema whose serialized size blows the byte cap is rejected with a size reason.
    #[test]
    fn oversized_schema_is_rejected() {
        // A wide object with many string properties easily exceeds the 32 KiB serialized cap.
        let mut props = serde_json::Map::new();
        for i in 0..4000 {
            props.insert(format!("field_{i}"), json!({ "type": "string" }));
        }
        let schema = json!({ "type": "object", "properties": props });
        let err = ensure_schema_within_bounds(&schema).expect_err("oversized schema must reject");
        assert!(
            err.contains("too large"),
            "reason must name the size: {err}"
        );
    }

    /// A schema nested past the depth cap is rejected with a depth reason.
    #[test]
    fn overdeep_schema_is_rejected() {
        // Build `{"a":{"a":{...}}}` nested well past the depth cap.
        let mut node = json!({ "type": "string" });
        for _ in 0..(WORKFLOW_SCHEMA_MAX_DEPTH + 5) {
            node = json!({ "type": "object", "properties": { "a": node } });
        }
        let err = ensure_schema_within_bounds(&node).expect_err("overdeep schema must reject");
        assert!(
            err.contains("too deep"),
            "reason must name the depth: {err}"
        );
    }

    /// `json_depth` counts nesting levels and short-circuits at the limit.
    #[test]
    fn json_depth_counts_levels() {
        assert_eq!(json_depth(&json!(1), 64), 1);
        assert_eq!(json_depth(&json!({ "a": 1 }), 64), 2);
        assert_eq!(json_depth(&json!({ "a": { "b": 1 } }), 64), 3);
        assert_eq!(json_depth(&json!([[[1]]]), 64), 4);
    }
}

#[cfg(test)]
mod finalize_agent_output_tests {
    use super::*;
    use serde_json::json;

    /// The structured-output schema used across these cases: an object requiring a string `answer`.
    fn answer_schema() -> JsonValue {
        json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        })
    }

    /// A schema fixture whose final text is conformant JSON returns the parsed object (not a string).
    #[test]
    fn schema_fixture_returns_validated_object() {
        let schema = answer_schema();
        let out = finalize_agent_output(Some(r#"{"answer":"42"}"#.to_string()), Some(&schema));
        assert_eq!(out, Some(json!({ "answer": "42" })));
        // Specifically an object, never the raw string.
        assert!(matches!(out, Some(JsonValue::Object(_))));
    }

    /// Well-formed JSON that violates the schema (wrong type) is rejected by the recheck → null.
    #[test]
    fn nonconformant_json_resolves_to_null() {
        let schema = answer_schema();
        // `answer` must be a string; a number is well-formed JSON but non-conformant.
        let out = finalize_agent_output(Some(r#"{"answer":42}"#.to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// A conformant-looking object with an extra key is rejected under `additionalProperties:false`.
    /// This is the belt-and-suspenders case: even if a non-strict engine let this through, our
    /// recheck still runs and rejects it.
    #[test]
    fn extra_property_rejected_by_recheck_even_if_engine_claims_strict() {
        let schema = answer_schema();
        let out = finalize_agent_output(
            Some(r#"{"answer":"ok","leaked":true}"#.to_string()),
            Some(&schema),
        );
        assert_eq!(out, None);
    }

    /// Malformed (non-JSON) final text under a schema resolves to null rather than throwing.
    #[test]
    fn malformed_json_resolves_to_null() {
        let schema = answer_schema();
        let out = finalize_agent_output(Some("not json at all".to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// An uncompilable JSON Schema is treated as a validation failure (null), never a trusted pass.
    #[test]
    fn uncompilable_schema_resolves_to_null() {
        // `type` must be a string/array of strings; a number makes the schema invalid.
        let schema = json!({ "type": 123 });
        let out = finalize_agent_output(Some(r#"{"answer":"ok"}"#.to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// Without a schema, `agent()` returns the plain final assistant text as a JSON string.
    #[test]
    fn without_schema_returns_plain_text_string() {
        let out = finalize_agent_output(Some("plain final text".to_string()), None);
        assert_eq!(out, Some(JsonValue::String("plain final text".to_string())));
    }

    /// Without a schema, text that merely *looks* like JSON is still returned verbatim as a string —
    /// no parsing happens on the schemaless path.
    #[test]
    fn without_schema_does_not_parse_jsonish_text() {
        let out = finalize_agent_output(Some(r#"{"a":1}"#.to_string()), None);
        assert_eq!(out, Some(JsonValue::String(r#"{"a":1}"#.to_string())));
    }

    /// A dead/aborted child (`None` final text) resolves to null with or without a schema.
    #[test]
    fn dead_agent_resolves_to_null() {
        let schema = answer_schema();
        assert_eq!(finalize_agent_output(None, Some(&schema)), None);
        assert_eq!(finalize_agent_output(None, None), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A workflow `agent()` call issues exactly one `DispatchMessage::SpawnAgent` (carrying the
    /// call's prompt/ordinal/opts) into the dispatch channel — one dispatch per `AgentCall`, never
    /// more.
    #[tokio::test]
    async fn spawn_agent_enqueues_one_spawn_agent_dispatch() {
        let broker = Arc::new(CodeModeDispatchBroker::new());

        // `spawn_agent` blocks awaiting a response the (absent) worker never sends, so drive it on a
        // task and inspect the message it enqueued on the shared dispatch channel.
        let call = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move {
                broker
                    .spawn_agent(
                        CellId::new("cell-1".to_string()),
                        "spawn me".to_string(),
                        7,
                        AgentCallOpts::default(),
                        CancellationToken::new(),
                    )
                    .await
            })
        };

        let message = tokio::time::timeout(Duration::from_secs(1), broker.dispatch_rx.recv())
            .await
            .expect("a dispatch message was enqueued")
            .expect("dispatch channel open");
        match message {
            DispatchMessage::SpawnAgent {
                cell_id,
                prompt,
                ordinal,
                ..
            } => {
                assert_eq!(cell_id, CellId::new("cell-1".to_string()));
                assert_eq!(prompt, "spawn me");
                assert_eq!(ordinal, 7);
            }
            _ => panic!("expected DispatchMessage::SpawnAgent"),
        }

        // Exactly one dispatch per `agent()` call: nothing else was enqueued.
        assert!(
            broker.dispatch_rx.try_recv().is_err(),
            "spawn_agent must enqueue exactly one dispatch message"
        );

        call.abort();
    }

    /// A cancelled `agent()` call resolves to `Failed` (JS null) without ever throwing.
    #[tokio::test]
    async fn spawn_agent_resolves_to_failed_when_cancelled_before_dispatch() {
        let broker = CodeModeDispatchBroker::new();
        let cancellation_token = CancellationToken::new();
        cancellation_token.cancel();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            broker.spawn_agent(
                CellId::new("cell-1".to_string()),
                "prompt".to_string(),
                0,
                AgentCallOpts::default(),
                cancellation_token,
            ),
        )
        .await
        .expect("spawn_agent resolved promptly");
        assert!(
            matches!(result, AgentSpawnOutcome::Failed),
            "a cancelled call must resolve to Failed (null), never throw"
        );
    }
}
