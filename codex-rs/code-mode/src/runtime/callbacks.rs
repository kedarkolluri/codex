use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::FunctionCallOutputContentItem;

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

    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    // Stamp the source-order ordinal SYNCHRONOUSLY before the promise returns,
    // so `Promise.all([agent(a),agent(b),agent(c)])` yields 0,1,2 in source
    // order regardless of host resolution order (§7).
    let ordinal = state.next_agent_ordinal;
    state.next_agent_ordinal = state.next_agent_ordinal.saturating_add(1);
    let id = format!("agent-{ordinal}");
    let event_tx = state.event_tx.clone();
    state.pending_tool_calls.insert(id.clone(), resolver);
    let _ = event_tx.send(RuntimeEvent::AgentCall {
        id,
        ordinal,
        prompt,
        opts,
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
}
