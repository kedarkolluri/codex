use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::ensure_workflow_agent_label;
use codex_code_mode_protocol::ensure_workflow_agent_option;
use codex_code_mode_protocol::ensure_workflow_agent_prompt;
use codex_code_mode_protocol::ensure_workflow_agent_schema;
use codex_code_mode_protocol::ensure_workflow_args;
use codex_code_mode_protocol::ensure_workflow_name;
use codex_code_mode_protocol::ensure_workflow_phase_title;
use codex_workflow_journal::AgentStatus;
use codex_workflow_journal::KeyInputs;

use super::super::RuntimeEvent;
use super::super::RuntimeState;
use super::super::value::throw_type_error;
use super::super::value::v8_value_to_json;

pub(in crate::runtime) fn tool_callback(
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
pub(in crate::runtime) fn agent_callback(
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
    if let Err(error_text) = ensure_workflow_agent_prompt(&prompt) {
        throw_type_error(scope, &error_text);
        return;
    }

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
    if let Some(label) = opts.label.as_deref()
        && let Err(error_text) = ensure_workflow_agent_label(label)
    {
        throw_type_error(scope, &error_text);
        return;
    }
    if let Some(phase) = opts.phase.as_deref()
        && let Err(error_text) = ensure_workflow_phase_title(phase)
    {
        throw_type_error(scope, &error_text);
        return;
    }
    for (field, value) in [
        ("workflow agent model", opts.model.as_deref()),
        ("workflow agent effort", opts.effort.as_deref()),
        ("workflow agent type", opts.agent_type.as_deref()),
        ("workflow agent isolation", opts.isolation.as_deref()),
    ] {
        if let Some(value) = value
            && let Err(error_text) = ensure_workflow_agent_option(field, value)
        {
            throw_type_error(scope, &error_text);
            return;
        }
    }
    if let Some(schema) = opts.schema.as_ref()
        && let Err(error_text) = ensure_workflow_agent_schema(schema)
    {
        throw_type_error(scope, &error_text);
        return;
    }

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
    let node_id = match state.allocate_workflow_node_id() {
        Ok(node_id) => node_id,
        Err(error_text) => {
            throw_type_error(scope, error_text);
            return;
        }
    };
    let parent_node_id = state.active_workflow_parent_node_id;
    let phase = opts.phase.clone().or_else(|| {
        state
            .active_workflow_phase
            .as_ref()
            .map(|(_, title)| title.clone())
    });
    let id = format!("agent-{ordinal}");
    let event_tx = state.event_tx.clone();
    state.active_workflow_nodes.insert(node_id);
    state
        .pending_workflow_agent_nodes
        .insert(id.clone(), node_id);

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

    if let Some(mut entry) = replay_entry {
        // Cache hit: serve from the journaled entry WITHOUT spawning a subagent.
        // `label` and `phase` are cosmetic and deliberately excluded from the cache key, so a
        // resumed script may change them without forcing divergence. Carry those current values to
        // the host (and the new run's journal) instead of replaying stale presentation metadata.
        entry.label.clone_from(&opts.label);
        entry.phase.clone_from(&opts.phase);
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
            node_id,
            parent_node_id,
            phase,
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
            node_id,
            parent_node_id,
            phase,
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
pub(in crate::runtime) fn workflow_callback(
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
    if let Err(error_text) = ensure_workflow_name(&name) {
        throw_type_error(scope, &error_text);
        return;
    }

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
    if let Some(workflow_args) = workflow_args.as_ref()
        && let Err(error_text) = ensure_workflow_args(workflow_args)
    {
        throw_type_error(scope, &error_text);
        return;
    }

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
