use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_workflow_journal::AgentStatus;
use codex_workflow_journal::KeyInputs;

use super::EXIT_SENTINEL;
use super::RuntimeEvent;
use super::RuntimeState;
use super::timers;
use super::value::json_to_v8;
use super::value::normalize_output_image;
use super::value::serialize_output_text;
use super::value::throw_type_error;
use super::value::v8_value_to_json;

pub(super) fn tool_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let tool_index = match args.data().to_rust_string_lossy(scope).parse::<usize>() {
        Ok(tool_index) => tool_index,
        Err(_) => {
            throw_type_error(scope, "invalid tool callback data");
            return;
        }
    };
    let input = if args.length() == 0 {
        Ok(None)
    } else {
        v8_value_to_json(scope, args.get(0))
    };
    let input = match input {
        Ok(input) => input,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_type_error(scope, "failed to create tool promise");
        return;
    };
    let promise = resolver.get_promise(scope);

    let resolver = v8::Global::new(scope, resolver);
    let (tool_name, tool_kind) = {
        let Some(state) = scope.get_slot::<RuntimeState>() else {
            throw_type_error(scope, "runtime state unavailable");
            return;
        };
        let Some(tool) = state.enabled_tools.get(tool_index) else {
            throw_type_error(scope, "tool callback data is out of range");
            return;
        };
        (tool.tool_name.clone(), tool.kind)
    };

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    let id = format!("tool-{}", state.next_tool_call_id);
    state.next_tool_call_id = state.next_tool_call_id.saturating_add(1);
    let event_tx = state.event_tx.clone();
    state.pending_tool_calls.insert(id.clone(), resolver);
    let _ = event_tx.send(RuntimeEvent::ToolCall {
        id,
        name: tool_name,
        kind: tool_kind,
        input,
    });
    retval.set(promise.into());
}

/// Workflow `agent(prompt, opts?)` global — spawns a subagent (§3 async bridge
/// op; §6 `agent()` mapping). Modeled exactly on [`tool_callback`]: it mints a
/// [`v8::PromiseResolver`], stamps `ordinal = state.next_agent_ordinal++`
/// SYNCHRONOUSLY before returning the promise (§7 invocation ordinal; matching
/// `tool_callback`'s `next_tool_call_id` bump), stores the `Global` resolver in
/// `pending_tool_calls` under a fresh id, and emits a
/// [`RuntimeEvent::AgentCall`] for the cell actor to route to the spawn helper.
/// Because the isolate is single-threaded and `parallel`/`Promise.all` fire
/// their thunks in array order up to the first `await`, the ordinal sequence is
/// deterministic in source order regardless of host response arrival order. No
/// host spawn wiring lives here — that is the `P1-cellactor-spawn-dispatch`
/// ticket. Installed only for workflow runs (see `globals::install_globals`).
pub(super) fn agent_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    // First positional argument: the prompt string. Require an actual string
    // rather than coercing arbitrary values so a misuse surfaces immediately.
    let prompt_value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    if !prompt_value.is_string() {
        throw_type_error(scope, "agent expects a prompt string");
        return;
    }
    let prompt = prompt_value.to_rust_string_lossy(scope);

    // Optional second argument: the opts object. `null`/`undefined`/absent all
    // map to the default (all-`None`) options. Unknown keys are ignored by
    // `AgentCallOpts` (no `deny_unknown_fields`).
    let opts = if args.length() < 2 {
        AgentCallOpts::default()
    } else {
        let opts_value = args.get(1);
        if opts_value.is_null() || opts_value.is_undefined() {
            AgentCallOpts::default()
        } else {
            match v8_value_to_json(scope, opts_value) {
                Ok(Some(json)) => match serde_json::from_value::<AgentCallOpts>(json) {
                    Ok(opts) => opts,
                    Err(error) => {
                        throw_type_error(scope, &format!("invalid agent options: {error}"));
                        return;
                    }
                },
                Ok(None) => {
                    throw_type_error(scope, "agent options must be a plain object");
                    return;
                }
                Err(error_text) => {
                    throw_type_error(scope, &error_text);
                    return;
                }
            }
        }
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_type_error(scope, "failed to create agent promise");
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    // Compute the `(prompt, opts)` cache key BEFORE borrowing state, for the §7
    // prefix-replay decision. `label`/`phase` are deliberately excluded (they are
    // not fields of `KeyInputs`), so cosmetic re-labeling never busts cache. The
    // isolate recomputes exactly the key the original run's host journaled from the
    // SAME raw `(prompt, opts)`, so an unchanged call at the same ordinal matches.
    let cache_key = KeyInputs {
        prompt: &prompt,
        model: opts.model.as_deref(),
        effort: opts.effort.as_deref(),
        agent_type: opts.agent_type.as_deref(),
        isolation: opts.isolation.as_deref(),
        schema: opts.schema.as_ref(),
    }
    .cache_key();

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    // Stamp the source-order ordinal SYNCHRONOUSLY before the promise returns,
    // so `Promise.all([agent(a),agent(b),agent(c)])` yields 0,1,2 in source
    // order regardless of host resolution order (§7). We key the replay cache on
    // this invocation ordinal, NEVER on completion order.
    let ordinal = state.next_agent_ordinal;
    state.next_agent_ordinal = state.next_agent_ordinal.saturating_add(1);
    let id = format!("agent-{ordinal}");
    let event_tx = state.event_tx.clone();

    // §7 "Resume algorithm" step 3, evaluated synchronously in ordinal order (the
    // isolate is single-threaded). That synchronous, source-ordered evaluation is
    // exactly what makes the "first divergence goes live and never returns to
    // replay" latch correct even inside a `Promise.all` batch — no barrier/no-barrier
    // special-casing. At ordinal `i` with computed key `k`, serve the journaled
    // result iff replay is still active, `i` is within the recorded prefix, the
    // entry's key matches `k`, AND it completed. A key mismatch, a missing entry, a
    // non-`completed` status, or exhausting the prefix all DIVERGE.
    let replay_entry = {
        let replay = state.replay();
        if replay.is_active() && ordinal < replay.prefix_len() {
            replay.entry(ordinal).and_then(|entry| {
                (entry.key == cache_key && entry.status == Some(AgentStatus::Completed))
                    .then(|| entry.clone())
            })
        } else {
            None
        }
    };

    if let Some(entry) = replay_entry {
        // Cache hit: serve from the journaled entry WITHOUT spawning a subagent.
        // Advance the isolate-side replay budget accumulator (the AUTHORITATIVE
        // shared-budget re-add + journal re-append happen host-side in
        // `CellHost::replay_agent`), keep the resolver keyed by `id` so the
        // `ToolResponse` resolve path settles it, and emit the replay event.
        state
            .replay_mut()
            .add_replay_spent(entry.tokens_spent.unwrap_or(0) as i64);
        state.pending_tool_calls.insert(id.clone(), resolver);
        let _ = event_tx.send(RuntimeEvent::AgentReplay {
            id,
            entry: Box::new(entry),
        });
    } else {
        // Divergence (or a fresh run): latch replay off PERMANENTLY — there is no
        // re-enable path, so a later ordinal whose key coincidentally matches still
        // runs live — and dispatch the call live via `AgentCall`.
        state.replay_mut().disable();
        state.pending_tool_calls.insert(id.clone(), resolver);
        let _ = event_tx.send(RuntimeEvent::AgentCall {
            id,
            ordinal,
            prompt,
            opts,
        });
    }
    retval.set(promise.into());
}

/// Workflow `workflow(nameOrRef, args)` global — runs another saved workflow
/// inline, one level deep (§4 `workflow()`). Modeled exactly on
/// [`agent_callback`]: it mints a [`v8::PromiseResolver`], stamps a fresh
/// `id = "workflow-{n}"` SYNCHRONOUSLY from `state.next_workflow_call_id++`
/// before returning the promise, stores the `Global` resolver in
/// `pending_tool_calls` under that id, and emits a [`RuntimeEvent::WorkflowCall`]
/// for the cell actor to route to the nested-run host handler. Because the
/// resolver is id-keyed in `pending_tool_calls`, the returned promise resolves
/// via the same response machinery as `agent()`/tool callbacks (out-of-order
/// safe). No host wiring lives here — the registry load + nested re-enter is the
/// `P2-workflow-registry-reenter` ticket. Installed only for workflow runs (see
/// `globals::install_globals`).
pub(super) fn workflow_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    // First positional argument: the workflow name/ref. Require an actual string
    // rather than coercing arbitrary values so a misuse surfaces immediately.
    let name_value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    if !name_value.is_string() {
        throw_type_error(scope, "workflow expects a name string");
        return;
    }
    let name = name_value.to_rust_string_lossy(scope);

    // Optional second argument: the args payload passed to the nested workflow.
    // `null`/`undefined`/absent all map to `None`; any other value is serialized
    // to JSON via `v8_value_to_json` (the same path `store()`/`agent()` opts use).
    let workflow_args = if args.length() < 2 {
        None
    } else {
        let args_value = args.get(1);
        if args_value.is_null() || args_value.is_undefined() {
            None
        } else {
            match v8_value_to_json(scope, args_value) {
                Ok(json) => json,
                Err(error_text) => {
                    throw_type_error(scope, &error_text);
                    return;
                }
            }
        }
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw_type_error(scope, "failed to create workflow promise");
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    // Stamp the id SYNCHRONOUSLY before the promise returns so the resolver is
    // uniquely keyed in `pending_tool_calls` regardless of host resolution order.
    let id = format!("workflow-{}", state.next_workflow_call_id);
    state.next_workflow_call_id = state.next_workflow_call_id.saturating_add(1);
    let event_tx = state.event_tx.clone();
    state.pending_tool_calls.insert(id.clone(), resolver);
    let _ = event_tx.send(RuntimeEvent::WorkflowCall {
        id,
        name,
        args: workflow_args,
    });
    retval.set(promise.into());
}

pub(super) fn text_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let text = match serialize_output_text(scope, value) {
        Ok(text) => text,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::ContentItem(
            FunctionCallOutputContentItem::InputText { text },
        ));
    }
    retval.set(v8::undefined(scope).into());
}

pub(super) fn image_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let detail_override = if args.length() < 2 {
        None
    } else {
        let detail = args.get(1);
        if detail.is_string() {
            Some(detail.to_rust_string_lossy(scope))
        } else if detail.is_null() || detail.is_undefined() {
            None
        } else {
            throw_type_error(scope, "image detail must be a string when provided");
            return;
        }
    };
    let image_item = match normalize_output_image(scope, value, detail_override) {
        Ok(image_item) => image_item,
        Err(()) => return,
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::ContentItem(image_item));
    }
    retval.set(v8::undefined(scope).into());
}

pub(super) fn generated_image_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let output_hint = match generated_image_output_hint(scope, value) {
        Ok(output_hint) => output_hint,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    let image_item = match normalize_output_image(scope, value, /*detail_override*/ None) {
        Ok(image_item) => image_item,
        Err(()) => return,
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::ContentItem(image_item));
        if let Some(text) = output_hint {
            let _ = state.event_tx.send(RuntimeEvent::ContentItem(
                FunctionCallOutputContentItem::InputText { text },
            ));
        }
    }
    retval.set(v8::undefined(scope).into());
}

fn generated_image_output_hint(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<String>, String> {
    let object = v8::Local::<v8::Object>::try_from(value)
        .map_err(|_| "generatedImage expects an image generation result object".to_string())?;
    let key = v8::String::new(scope, "output_hint")
        .ok_or_else(|| "failed to allocate generatedImage helper keys".to_string())?;
    let output_hint = object
        .get(scope, key.into())
        .ok_or_else(|| "failed to read generatedImage output_hint".to_string())?;
    if output_hint.is_undefined() {
        return Ok(None);
    }
    if !output_hint.is_string() {
        return Err("generatedImage output_hint must be a string when provided".to_string());
    }
    Ok(Some(output_hint.to_rust_string_lossy(scope)))
}

pub(super) fn store_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    let key = match args.get(0).to_string(scope) {
        Some(key) => key.to_rust_string_lossy(scope),
        None => {
            throw_type_error(scope, "store key must be a string");
            return;
        }
    };
    let value = args.get(1);
    let serialized = match v8_value_to_json(scope, value) {
        Ok(Some(value)) => value,
        Ok(None) => {
            throw_type_error(
                scope,
                &format!("Unable to store {key:?}. Only plain serializable objects can be stored."),
            );
            return;
        }
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.stored_values.insert(key.clone(), serialized.clone());
        state.stored_value_writes.insert(key, serialized);
    }
}

pub(super) fn load_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let key = match args.get(0).to_string(scope) {
        Some(key) => key.to_rust_string_lossy(scope),
        None => {
            throw_type_error(scope, "load key must be a string");
            return;
        }
    };
    let value = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.stored_values.get(&key))
        .cloned();
    let Some(value) = value else {
        retval.set(v8::undefined(scope).into());
        return;
    };
    let Some(value) = json_to_v8(scope, &value) else {
        throw_type_error(scope, "failed to load stored value");
        return;
    };
    retval.set(value);
}

/// Shared text extraction for the narrator-style globals (`notify`, `log`,
/// `phase`). Serializes the first argument to text, rejecting empty input with
/// an actionable, per-helper error. Returns `None` after throwing so callers can
/// simply `return`. Centralizing this keeps `log()`/`phase()` as thin aliases
/// over the existing `notify` plumbing rather than duplicating it.
fn narrator_text(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    helper: &str,
) -> Option<String> {
    let value = if args.length() == 0 {
        v8::undefined(scope).into()
    } else {
        args.get(0)
    };
    let text = match serialize_output_text(scope, value) {
        Ok(text) => text,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return None;
        }
    };
    if text.trim().is_empty() {
        throw_type_error(scope, &format!("{helper} expects non-empty text"));
        return None;
    }
    Some(text)
}

pub(super) fn notify_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(text) = narrator_text(scope, &args, "notify") else {
        return;
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::Notify {
            call_id: state.tool_call_id.clone(),
            text,
        });
    }
    retval.set(v8::undefined(scope).into());
}

/// Workflow `log(msg)` global — a thin alias over the `notify` text path that
/// emits a distinct [`RuntimeEvent::WorkflowLog`]. Installed only for workflow
/// runs (see `globals::install_globals`).
pub(super) fn log_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(message) = narrator_text(scope, &args, "log") else {
        return;
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::WorkflowLog { message });
    }
    retval.set(v8::undefined(scope).into());
}

/// Workflow `phase(title)` global — emits a [`RuntimeEvent::Phase`] progress
/// marker. Installed only for workflow runs (see `globals::install_globals`).
pub(super) fn phase_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(title) = narrator_text(scope, &args, "phase") else {
        return;
    };
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::Phase { title });
    }
    retval.set(v8::undefined(scope).into());
}

pub(super) fn set_timeout_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let timeout_id = match timers::schedule_timeout(scope, args) {
        Ok(timeout_id) => timeout_id,
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            return;
        }
    };

    retval.set(v8::Number::new(scope, timeout_id as f64).into());
}

pub(super) fn clear_timeout_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    if let Err(error_text) = timers::clear_timeout(scope, args) {
        throw_type_error(scope, &error_text);
        return;
    }

    retval.set(v8::undefined(scope).into());
}

pub(super) fn yield_control_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    if let Some(state) = scope.get_slot::<RuntimeState>() {
        let _ = state.event_tx.send(RuntimeEvent::YieldRequested);
    }
}

pub(super) fn exit_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.exit_requested = true;
    }
    if let Some(error) = v8::String::new(scope, EXIT_SENTINEL) {
        scope.throw_exception(error.into());
    }
}

#[cfg(test)]
mod tests {
    //! Isolate-level tests for the workflow `agent()` global (P1-agent-callback).
    //! These drive the real V8 runtime through [`spawn_runtime`] so the
    //! synchronous ordinal stamping and id-keyed resolution are exercised
    //! end-to-end, exactly as the acceptance criteria name.
    use std::collections::HashMap;
    use std::time::Duration;

    use codex_code_mode_protocol::AgentCallOpts;
    use codex_code_mode_protocol::ExecuteRequest;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::super::PendingRuntimeMode;
    use super::super::RuntimeCommand;
    use super::super::RuntimeEvent;
    use super::super::spawn_runtime;
    use crate::FunctionCallOutputContentItem;

    /// A workflow-mode request — the explicit `workflow` flag is the only thing
    /// that authorizes the `agent` global.
    fn workflow_execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
            workflow: true,
            args: None,
            run_id: None,
        }
    }

    /// A plain (non-workflow) request: identical plumbing but without the flag,
    /// so the `agent` global is never installed.
    fn plain_execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            workflow: false,
            ..workflow_execute_request(source)
        }
    }

    async fn next_event(event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed")
    }

    /// Collect the first `count` [`RuntimeEvent::AgentCall`] events, skipping the
    /// `Started`/`Pending` lifecycle events, and return their `(id, ordinal,
    /// prompt, opts)` tuples in emission order.
    async fn collect_agent_calls(
        event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
        count: usize,
    ) -> Vec<(String, u64, String, AgentCallOpts)> {
        let mut calls = Vec::new();
        while calls.len() < count {
            match next_event(event_rx).await {
                RuntimeEvent::AgentCall {
                    id,
                    ordinal,
                    prompt,
                    opts,
                } => calls.push((id, ordinal, prompt, opts)),
                RuntimeEvent::Started | RuntimeEvent::Pending => {}
                other => panic!("unexpected event before agent calls: {other:?}"),
            }
        }
        calls
    }

    /// Drain events until the runtime reports its terminal `Result`, returning
    /// the ordered events observed (including the final `Result`).
    async fn drain_to_result(
        event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let mut events = Vec::new();
        loop {
            let event = next_event(event_rx).await;
            let is_result = matches!(event, RuntimeEvent::Result { .. });
            events.push(event);
            if is_result {
                return events;
            }
        }
    }

    #[tokio::test]
    async fn agent_call_emits_one_pending_promise_event_resolvable_by_id() {
        // `agent("p")` returns a pending Promise and emits exactly one AgentCall.
        // Responding by the emitted id resolves that promise (proving the
        // resolver is retrievable by id from `pending_tool_calls`).
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request("text(await agent('solve'));"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let calls = collect_agent_calls(&mut event_rx, 1).await;
        assert_eq!(calls.len(), 1);
        let (id, ordinal, prompt, opts) = &calls[0];
        assert_eq!(id, "agent-0");
        assert_eq!(*ordinal, 0);
        assert_eq!(prompt, "solve");
        assert_eq!(*opts, AgentCallOpts::default());

        runtime_tx
            .send(RuntimeCommand::ToolResponse {
                id: id.clone(),
                result: json!("done"),
            })
            .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        // Exactly one AgentCall over the whole run.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::AgentCall { .. }))
                .count(),
            0,
            "the single AgentCall was already consumed before draining: {events:?}"
        );
        let text = events.iter().find_map(|event| match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                Some(text.clone())
            }
            _ => None,
        });
        assert_eq!(text.as_deref(), Some("done"));
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(error_text.is_none(), "workflow body must run cleanly");
    }

    #[tokio::test]
    async fn parallel_agent_calls_stamp_source_order_ordinals_regardless_of_arrival() {
        // Promise.all fires the three agent() thunks synchronously in array
        // order, so ordinals are 0,1,2 in SOURCE order. Responding in REVERSE
        // arrival order must not perturb the ordinals, and Promise.all preserves
        // the source-order mapping of results.
        let source = r#"
const results = await Promise.all([
  agent('a', { label: 'la' }),
  agent('b', { phase: 'ph', agentType: 'reviewer' }),
  agent('c'),
]);
text(results.join(','));
"#;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let calls = collect_agent_calls(&mut event_rx, 3).await;
        let ids: Vec<&str> = calls.iter().map(|(id, ..)| id.as_str()).collect();
        let ordinals: Vec<u64> = calls.iter().map(|(_, ordinal, ..)| *ordinal).collect();
        let prompts: Vec<&str> = calls
            .iter()
            .map(|(_, _, prompt, _)| prompt.as_str())
            .collect();

        assert_eq!(ordinals, vec![0, 1, 2], "ordinals must be source-ordered");
        assert_eq!(ids, vec!["agent-0", "agent-1", "agent-2"]);
        assert_eq!(prompts, vec!["a", "b", "c"]);

        // opts fields are carried through unchanged onto the emitted event.
        assert_eq!(calls[0].3.label.as_deref(), Some("la"));
        assert_eq!(calls[1].3.phase.as_deref(), Some("ph"));
        assert_eq!(calls[1].3.agent_type.as_deref(), Some("reviewer"));
        assert_eq!(calls[2].3, AgentCallOpts::default());

        // Respond in reverse arrival order: agent-2, then agent-1, then agent-0.
        for (id, result) in [
            ("agent-2", json!("C")),
            ("agent-1", json!("B")),
            ("agent-0", json!("A")),
        ] {
            runtime_tx
                .send(RuntimeCommand::ToolResponse {
                    id: id.to_string(),
                    result,
                })
                .unwrap();
        }

        let events = drain_to_result(&mut event_rx).await;
        let text = events.iter().find_map(|event| match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                Some(text.clone())
            }
            _ => None,
        });
        // Promise.all preserves source-order mapping despite reverse resolution.
        assert_eq!(text.as_deref(), Some("A,B,C"));
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(error_text.is_none(), "workflow body must run cleanly");
    }

    #[tokio::test]
    async fn plain_exec_has_no_agent_global() {
        // Workflow-ness is the explicit flag, never the source shape: a plain
        // exec calling `agent()` sees a ReferenceError.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            plain_execute_request("await agent('x');"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::AgentCall { .. })),
            "plain exec must not emit agent calls: {events:?}"
        );
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("agent is not defined"),
            "expected ReferenceError for missing `agent` global, got: {error_text}"
        );
    }

    /// Collect the first `count` [`RuntimeEvent::WorkflowCall`] events, skipping
    /// the `Started`/`Pending` lifecycle events, and return their `(id, name,
    /// args)` tuples in emission order.
    async fn collect_workflow_calls(
        event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
        count: usize,
    ) -> Vec<(String, String, Option<serde_json::Value>)> {
        let mut calls = Vec::new();
        while calls.len() < count {
            match next_event(event_rx).await {
                RuntimeEvent::WorkflowCall { id, name, args } => calls.push((id, name, args)),
                RuntimeEvent::Started | RuntimeEvent::Pending => {}
                other => panic!("unexpected event before workflow calls: {other:?}"),
            }
        }
        calls
    }

    #[tokio::test]
    async fn workflow_call_emits_one_pending_promise_event_resolvable_by_id() {
        // `workflow('child')` returns a pending Promise and emits exactly one
        // WorkflowCall with a stamped id, the resolved name, and (absent second
        // arg) `args = None`. Responding by the emitted id resolves that promise,
        // proving the resolver is retrievable by id from `pending_tool_calls`.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request("text(await workflow('child'));"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let calls = collect_workflow_calls(&mut event_rx, 1).await;
        assert_eq!(calls.len(), 1);
        let (id, name, args) = &calls[0];
        assert_eq!(id, "workflow-0");
        assert_eq!(name, "child");
        assert_eq!(*args, None);

        runtime_tx
            .send(RuntimeCommand::ToolResponse {
                id: id.clone(),
                result: json!("child-result"),
            })
            .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        // The single WorkflowCall was already consumed before draining.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::WorkflowCall { .. }))
                .count(),
            0,
            "the single WorkflowCall was already consumed before draining: {events:?}"
        );
        let text = events.iter().find_map(|event| match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                Some(text.clone())
            }
            _ => None,
        });
        assert_eq!(text.as_deref(), Some("child-result"));
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(error_text.is_none(), "workflow body must run cleanly");
    }

    #[tokio::test]
    async fn workflow_calls_carry_args_and_resolve_out_of_order() {
        // Two `workflow()` calls under Promise.all: each emits a distinct stamped
        // id and carries its args JSON verbatim. Responding in REVERSE arrival
        // order still resolves each promise by id (out-of-order safe, same
        // machinery as agent()/tool callbacks).
        let source = r#"
const results = await Promise.all([
  workflow('first', { n: 1 }),
  workflow('second', { n: 2, tags: ['x'] }),
]);
text(results.join(','));
"#;
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let calls = collect_workflow_calls(&mut event_rx, 2).await;
        let ids: Vec<&str> = calls.iter().map(|(id, ..)| id.as_str()).collect();
        let names: Vec<&str> = calls.iter().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(ids, vec!["workflow-0", "workflow-1"]);
        assert_eq!(names, vec!["first", "second"]);
        assert_eq!(calls[0].2, Some(json!({ "n": 1 })));
        assert_eq!(calls[1].2, Some(json!({ "n": 2, "tags": ["x"] })));

        // Respond in reverse arrival order: workflow-1 then workflow-0.
        for (id, result) in [("workflow-1", json!("B")), ("workflow-0", json!("A"))] {
            runtime_tx
                .send(RuntimeCommand::ToolResponse {
                    id: id.to_string(),
                    result,
                })
                .unwrap();
        }

        let events = drain_to_result(&mut event_rx).await;
        let text = events.iter().find_map(|event| match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                Some(text.clone())
            }
            _ => None,
        });
        // Promise.all preserves source-order mapping despite reverse resolution.
        assert_eq!(text.as_deref(), Some("A,B"));
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(error_text.is_none(), "workflow body must run cleanly");
    }

    #[tokio::test]
    async fn plain_exec_has_no_workflow_global() {
        // Workflow-ness is the explicit flag, never the source shape: a plain
        // exec calling `workflow()` sees a ReferenceError and emits no
        // WorkflowCall.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            plain_execute_request("await workflow('child');"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::WorkflowCall { .. })),
            "plain exec must not emit workflow calls: {events:?}"
        );
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("workflow is not defined"),
            "expected ReferenceError for missing `workflow` global, got: {error_text}"
        );
    }
}

/// Isolate-level tests for the `P3-resume-prefix-loop` decision in
/// [`agent_callback`]. They seed the runtime with a prior run's journal
/// `agent_call` entries (as `P3-resume-entry` will in production) and drive the
/// real V8 runtime, emulating the cell actor: an `AgentReplay` (cache hit) is
/// settled from the journaled return WITHOUT a "live" spawn, while an `AgentCall`
/// (divergence) is settled by the test's stand-in host. Asserting on which
/// ordinals produced `AgentReplay` vs `AgentCall` is exactly the acceptance
/// criteria ("no subagent spawns", "the divergence ordinal", the latch, and the
/// source-order `Promise.all` mapping).
#[cfg(test)]
mod replay_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use codex_code_mode_protocol::AgentCallOpts;
    use codex_code_mode_protocol::ExecuteRequest;
    use codex_code_mode_protocol::WorkflowBudgetHandle;
    use codex_workflow_journal::AgentCallLine;
    use codex_workflow_journal::AgentCallOpts as JournalOpts;
    use codex_workflow_journal::AgentStatus;
    use codex_workflow_journal::KeyInputs;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::super::PendingRuntimeMode;
    use super::super::RuntimeCommand;
    use super::super::RuntimeEvent;
    use super::super::spawn_runtime_with_budget;
    use crate::FunctionCallOutputContentItem;

    /// How a given ordinal was settled: served from the replay cache (no spawn) or
    /// dispatched live.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Served {
        Replay,
        Live,
    }

    /// A [`WorkflowBudgetHandle`] whose `spent` counter the replay driver bumps by
    /// each cache hit's `tokens_spent` — the isolate-level stand-in for the
    /// host-side `RolloutBudget::add_spent` re-add so `budget.spent()` observed by
    /// the resumed script matches the original run byte-for-byte.
    struct ReplayBudget {
        total: i64,
        spent: AtomicI64,
    }

    impl ReplayBudget {
        fn new(total: i64) -> Arc<Self> {
            Arc::new(Self {
                total,
                spent: AtomicI64::new(0),
            })
        }

        fn add_spent(&self, tokens: i64) {
            self.spent.fetch_add(tokens, Ordering::SeqCst);
        }
    }

    impl WorkflowBudgetHandle for ReplayBudget {
        fn total(&self) -> i64 {
            self.total
        }
        fn spent(&self) -> i64 {
            self.spent.load(Ordering::SeqCst)
        }
        fn remaining(&self) -> i64 {
            (self.total - self.spent()).max(0)
        }
    }

    fn workflow_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
            workflow: true,
            args: None,
            run_id: None,
        }
    }

    /// The `(prompt, opts)` cache key exactly as [`agent_callback`] computes it, so
    /// a seeded entry keyed with this matches a same-`(prompt, opts)` invocation.
    fn cache_key(prompt: &str, opts: &AgentCallOpts) -> String {
        KeyInputs {
            prompt,
            model: opts.model.as_deref(),
            effort: opts.effort.as_deref(),
            agent_type: opts.agent_type.as_deref(),
            isolation: opts.isolation.as_deref(),
            schema: opts.schema.as_ref(),
        }
        .cache_key()
    }

    /// A journaled `agent_call` entry at `ordinal` for a default-opts `prompt`,
    /// with `status`, `ret`, and `tokens_spent`. `key` is computed so it matches a
    /// same-prompt invocation; a `completed` entry carries the child linkage §7
    /// validation requires.
    fn entry(
        ordinal: u64,
        prompt: &str,
        status: Option<AgentStatus>,
        ret: serde_json::Value,
        tokens: Option<u64>,
    ) -> AgentCallLine {
        let completed = status == Some(AgentStatus::Completed);
        AgentCallLine {
            timestamp: None,
            ordinal,
            key: cache_key(prompt, &AgentCallOpts::default()),
            prompt_hash: "blake3:test".to_string(),
            opts: JournalOpts {
                model: None,
                effort: None,
                agent_type: None,
                isolation: None,
                schema_hash: None,
            },
            phase: None,
            label: None,
            child_thread_id: completed.then(|| "th_prior".to_string()),
            rollout_path: completed.then(|| "/prior/rollout.jsonl".to_string()),
            status,
            ret,
            tokens_spent: tokens,
            completion_seq: None,
        }
    }

    /// A `completed` entry (the common seed shape).
    fn completed(ordinal: u64, prompt: &str, ret: serde_json::Value, tokens: u64) -> AgentCallLine {
        entry(
            ordinal,
            prompt,
            Some(AgentStatus::Completed),
            ret,
            Some(tokens),
        )
    }

    async fn recv(event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed")
    }

    /// Drive a seeded-replay runtime to its `Result`, settling every agent event
    /// the way the cell actor + host would: an `AgentReplay` cache hit resolves
    /// from the journaled `return` (and, with a budget, re-adds its `tokens_spent`)
    /// WITHOUT a spawn; a live `AgentCall` resolves from `live(prompt)`. Returns
    /// the ordered `(ordinal, Served)` log and the collected `text(...)` outputs.
    async fn drive(
        event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
        runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
        live: impl Fn(&str) -> serde_json::Value,
        budget: Option<&Arc<ReplayBudget>>,
    ) -> (Vec<(u64, Served)>, Vec<String>) {
        let mut served = Vec::new();
        let mut text = Vec::new();
        loop {
            let event = recv(event_rx).await;
            match event {
                RuntimeEvent::AgentReplay { id, entry } => {
                    served.push((entry.ordinal, Served::Replay));
                    if let Some(budget) = budget {
                        budget.add_spent(entry.tokens_spent.unwrap_or(0) as i64);
                    }
                    runtime_tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: entry.ret,
                        })
                        .unwrap();
                }
                RuntimeEvent::AgentCall {
                    id,
                    ordinal,
                    prompt,
                    ..
                } => {
                    served.push((ordinal, Served::Live));
                    runtime_tx
                        .send(RuntimeCommand::ToolResponse {
                            id,
                            result: live(&prompt),
                        })
                        .unwrap();
                }
                RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text: t }) => {
                    text.push(t);
                }
                RuntimeEvent::Result { error_text, .. } => {
                    assert!(error_text.is_none(), "workflow body must run cleanly");
                    break;
                }
                _ => {}
            }
        }
        (served, text)
    }

    fn spawn(
        source: &str,
        replay_entries: Vec<AgentCallLine>,
        budget: Option<Arc<ReplayBudget>>,
    ) -> (
        std::sync::mpsc::Sender<RuntimeCommand>,
        mpsc::UnboundedReceiver<RuntimeEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _ctrl, _handle) = spawn_runtime_with_budget(
            HashMap::new(),
            workflow_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
            budget.map(|budget| budget as Arc<dyn WorkflowBudgetHandle>),
            Some(replay_entries),
        )
        .unwrap();
        (runtime_tx, event_rx)
    }

    fn live_marker(prompt: &str) -> serde_json::Value {
        json!(format!("live:{prompt}"))
    }

    #[tokio::test]
    async fn identical_prefix_replays_every_ordinal_with_no_live_spawns() {
        // Acceptance: an identical script + args replays the whole prefix from
        // cache with NO subagent spawns (zero live `AgentCall`).
        let source = r#"
const a = await agent('a');
const b = await agent('b');
text([a, b].join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            completed(1, "b", json!("B"), 20),
        ];
        let (runtime_tx, mut event_rx) = spawn(source, entries, None);
        let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

        assert_eq!(served, vec![(0, Served::Replay), (1, Served::Replay)]);
        assert!(
            served.iter().all(|(_, kind)| *kind == Served::Replay),
            "no ordinal may dispatch live on a full-prefix replay: {served:?}"
        );
        // Returns come from the journal, not the live stand-in.
        assert_eq!(text, vec!["A,B".to_string()]);
    }

    #[tokio::test]
    async fn edited_call_replays_the_unchanged_prefix_then_goes_live() {
        // Acceptance: the longest unchanged prefix resolves from cache and the
        // first changed call PLUS everything after it runs live. Prior prompts
        // were [a, b, c]; the edited script changes ordinal 1 to `X`. Ordinal 2's
        // prompt `c` still matches the seed but must run live (latch).
        let source = r#"
const r0 = await agent('a');
const r1 = await agent('X');
const r2 = await agent('c');
text([r0, r1, r2].join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            completed(1, "b", json!("B"), 20),
            completed(2, "c", json!("C"), 30),
        ];
        let (runtime_tx, mut event_rx) = spawn(source, entries, None);
        let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

        assert_eq!(
            served,
            vec![(0, Served::Replay), (1, Served::Live), (2, Served::Live)],
            "divergence is at ordinal 1; ordinal 2 runs live despite a matching key",
        );
        assert_eq!(text, vec!["A,live:X,live:c".to_string()]);
    }

    #[tokio::test]
    async fn non_completed_status_forces_divergence_at_that_ordinal() {
        // Acceptance: a cached entry whose `status != completed` forces divergence
        // at that ordinal even though the prompt/key are unchanged.
        let source = r#"
const r0 = await agent('a');
const r1 = await agent('b');
text([r0, r1].join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            // In-flight/interrupted at resume time: status is null, so it re-runs.
            entry(1, "b", None, json!(null), None),
        ];
        let (runtime_tx, mut event_rx) = spawn(source, entries, None);
        let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

        assert_eq!(served, vec![(0, Served::Replay), (1, Served::Live)]);
        assert_eq!(text, vec!["A,live:b".to_string()]);
    }

    #[tokio::test]
    async fn replay_never_re_enables_after_the_first_divergence() {
        // Acceptance: `replay_active` flips false at the first divergence and never
        // re-enables, even when a LATER ordinal's key coincidentally matches. Here
        // ordinal 0 diverges (prompt `Z` != seeded `a`); ordinals 1 and 2 keep the
        // seeded prompts (matching keys) yet all run live.
        let source = r#"
const r0 = await agent('Z');
const r1 = await agent('b');
const r2 = await agent('c');
text([r0, r1, r2].join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            completed(1, "b", json!("B"), 20),
            completed(2, "c", json!("C"), 30),
        ];
        let (runtime_tx, mut event_rx) = spawn(source, entries, None);
        let (served, _text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

        assert_eq!(
            served,
            vec![(0, Served::Live), (1, Served::Live), (2, Served::Live)],
            "a divergence at ordinal 0 forces every later ordinal live despite matching keys",
        );
    }

    #[tokio::test]
    async fn parallel_batch_replays_each_ordinal_in_source_order() {
        // Acceptance: a `Promise.all` batch during replay resolves each cached
        // ordinal in source order (deterministic), matching the original run's
        // results position-for-position — barrier/no-barrier reproduced with no
        // special-casing.
        let source = r#"
const results = await Promise.all([agent('a'), agent('b'), agent('c')]);
text(results.join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            completed(1, "b", json!("B"), 20),
            completed(2, "c", json!("C"), 30),
        ];
        let (runtime_tx, mut event_rx) = spawn(source, entries, None);
        let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, None).await;

        assert_eq!(
            served,
            vec![
                (0, Served::Replay),
                (1, Served::Replay),
                (2, Served::Replay)
            ],
            "the whole batch is a full-prefix hit, issued in source order",
        );
        assert_eq!(
            text,
            vec!["A,B,C".to_string()],
            "Promise.all preserves the source-order result mapping across replay",
        );
    }

    #[tokio::test]
    async fn replayed_prefix_budget_is_byte_identical() {
        // Acceptance: `budget.spent()` / `budget.remaining()` after the replayed
        // prefix are byte-identical to the original run. The driver re-adds each
        // cache hit's journaled `tokens_spent` (the isolate stand-in for the
        // host-side `RolloutBudget::add_spent`), so the script observes the
        // original run's spend curve.
        let source = r#"
const a = await agent('a');
const b = await agent('b');
text(String(budget.spent()));
text(String(budget.remaining()));
text([a, b].join(','));
"#;
        let entries = vec![
            completed(0, "a", json!("A"), 10),
            completed(1, "b", json!("B"), 20),
        ];
        let budget = ReplayBudget::new(100);
        let (runtime_tx, mut event_rx) = spawn(source, entries, Some(Arc::clone(&budget)));
        let (served, text) = drive(&mut event_rx, &runtime_tx, live_marker, Some(&budget)).await;

        assert_eq!(served, vec![(0, Served::Replay), (1, Served::Replay)]);
        // 10 + 20 re-added during replay: spent 30 of 100, remaining 70.
        assert_eq!(
            text,
            vec!["30".to_string(), "70".to_string(), "A,B".to_string()],
        );
    }
}

/// Property/fuzz coverage for the §7 invocation-ordinal spine (`P3-uat-resume-gate`
/// acceptance: "property/fuzz test over random parallel/pipeline shapes confirms
/// ordinal determinism"). The cache spine keys on the SOURCE-ORDER invocation
/// ordinal stamped synchronously in [`agent_callback`], never on completion order.
/// These drive fresh (non-resume) fan-outs of random width through the real V8
/// runtime, resolve them in a random (out-of-order) completion order, and assert
/// the emitted `AgentCall` ordinals are STILL the strict source-order sequence
/// `0..N` — the invariant that makes prefix-replay cache keys reproducible run to
/// run regardless of which subagent settles first. Randomness is a deterministic
/// splitmix64 PRNG (no wall-clock, no `std` rng), honouring the §7 contract.
#[cfg(test)]
mod ordinal_determinism_property_tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use codex_code_mode_protocol::ExecuteRequest;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::super::PendingRuntimeMode;
    use super::super::RuntimeCommand;
    use super::super::RuntimeEvent;
    use super::super::spawn_runtime;
    use crate::FunctionCallOutputContentItem;

    fn workflow_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
            workflow: true,
            args: None,
            run_id: None,
        }
    }

    /// A deterministic splitmix64 step — the only entropy source these tests use.
    fn next_rand(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fisher-Yates a `0..len` permutation with the deterministic PRNG.
    fn permutation(len: usize, state: &mut u64) -> Vec<usize> {
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = (next_rand(state) as usize) % (i + 1);
            order.swap(i, j);
        }
        order
    }

    /// Drive a fresh `parallel()` fan-out of `width` agents (prompts `p0..p{width-1}`),
    /// buffering every `AgentCall` until all `width` have dispatched, then resolving them
    /// in `response_order` (a permutation of `0..width`) — an out-of-order completion.
    /// Returns the `(ordinal, prompt)` pairs in the order the runtime EMITTED them and the
    /// workflow's own result array. Each agent resolves to its own prompt so the result
    /// array position-check is independent of completion order.
    async fn run_parallel(
        width: usize,
        response_order: &[usize],
    ) -> (Vec<(u64, String)>, Vec<String>) {
        let source = format!(
            "export const meta = {{ name: 'demo', description: 'demo' }};\n\
             const thunks = [];\n\
             for (let i = 0; i < {width}; i++) thunks.push(((k) => () => agent('p' + k))(i));\n\
             const results = await parallel(thunks);\n\
             text(JSON.stringify(results));\n",
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_request(&source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        // Collect all `width` synchronously-dispatched AgentCalls before resolving any,
        // recording emission order; a parallel batch fires every thunk (and thus every
        // `agent()`) up to the first await, so all AgentCalls arrive before responses.
        let mut emitted: Vec<(u64, String)> = Vec::new();
        let mut pending: Vec<(String, serde_json::Value)> = Vec::new();
        while emitted.len() < width {
            let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .expect("runtime event timeout")
                .expect("runtime event channel closed");
            if let RuntimeEvent::AgentCall {
                id,
                ordinal,
                prompt,
                ..
            } = event
            {
                emitted.push((ordinal, prompt.clone()));
                pending.push((id, json!(prompt)));
            }
        }

        // Resolve in the (out-of-order) completion permutation.
        for &idx in response_order {
            let (id, result) = pending[idx].clone();
            runtime_tx
                .send(RuntimeCommand::ToolResponse { id, result })
                .unwrap();
        }

        // Drain the remaining events to the terminal Result, collecting text output.
        let mut text = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .expect("runtime event timeout")
                .expect("runtime event channel closed");
            match event {
                RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text: t }) => {
                    text.push(t);
                }
                RuntimeEvent::Result { error_text, .. } => {
                    assert!(
                        error_text.is_none(),
                        "fan-out must run cleanly: {error_text:?}"
                    );
                    break;
                }
                _ => {}
            }
        }
        (emitted, text)
    }

    #[tokio::test]
    async fn random_parallel_widths_stamp_ordinals_in_source_order_despite_completion_order() {
        // Over many random fan-out widths resolved in random completion orders, the emitted
        // `AgentCall` ordinals are ALWAYS the strict source-order `0..N` and the result array is
        // position-preserving — ordinal assignment is keyed on dispatch order, never completion.
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        for _ in 0..40 {
            let width = 1 + (next_rand(&mut state) as usize) % 8; // 1..=8
            let order = permutation(width, &mut state);
            let (emitted, text) = run_parallel(width, &order).await;

            let ordinals: Vec<u64> = emitted.iter().map(|(ordinal, _)| *ordinal).collect();
            let expected: Vec<u64> = (0..width as u64).collect();
            assert_eq!(
                ordinals, expected,
                "ordinals must be source-order 0..N regardless of completion order {order:?}",
            );
            let prompts: Vec<&str> = emitted.iter().map(|(_, p)| p.as_str()).collect();
            let expected_prompts: Vec<String> = (0..width).map(|k| format!("p{k}")).collect();
            assert_eq!(
                prompts,
                expected_prompts
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                "the ordinal->prompt mapping follows source order",
            );

            // The workflow result is position-preserving: results[k] == the k-th prompt, even
            // though the agents settled in `order`.
            let expected_json = serde_json::to_string(&expected_prompts).unwrap();
            assert_eq!(
                text,
                vec![expected_json],
                "results stay position-preserving"
            );
        }
    }

    #[tokio::test]
    async fn same_shape_stamps_identical_ordinals_under_different_completion_orders() {
        // Determinism: the SAME fan-out shape run twice under DIFFERENT completion orders emits a
        // byte-identical `(ordinal, prompt)` sequence — the cache spine is reproducible run to run,
        // which is the precondition for a resumed run's keys to match the journaled ones.
        let width = 6;
        let (emitted_a, _) = run_parallel(width, &[0, 1, 2, 3, 4, 5]).await;
        let (emitted_b, _) = run_parallel(width, &[5, 4, 3, 2, 1, 0]).await;
        assert_eq!(
            emitted_a, emitted_b,
            "completion order must not perturb the invocation-ordinal spine",
        );
    }
}
