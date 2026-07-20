use codex_code_mode_protocol::ensure_workflow_log_message;
use codex_code_mode_protocol::ensure_workflow_phase_title;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;

use super::super::RuntimeEvent;
use super::super::RuntimeState;
use super::super::value::serialize_output_text;
use super::super::value::throw_type_error;

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

pub(in crate::runtime) fn notify_callback(
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
pub(in crate::runtime) fn log_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(message) = narrator_text(scope, &args, "log") else {
        return;
    };
    if let Err(error_text) = ensure_workflow_log_message(&message) {
        throw_type_error(scope, &error_text);
        return;
    }
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        if let Err(error_text) = state.admit_workflow_log() {
            throw_type_error(scope, error_text);
            return;
        }
        let _ = state.event_tx.send(RuntimeEvent::WorkflowLog {
            message: message.clone(),
        });
        if let Some(run_id) = state.run_id.clone() {
            state.emit_workflow_progress(WorkflowEvent::Log(WorkflowLogEvent { run_id, message }));
        }
    }
    retval.set(v8::undefined(scope).into());
}

/// Workflow `phase(title)` global — emits a [`RuntimeEvent::Phase`] progress
/// marker. Installed only for workflow runs (see `globals::install_globals`).
pub(in crate::runtime) fn phase_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(title) = narrator_text(scope, &args, "phase") else {
        return;
    };
    if let Err(error_text) = ensure_workflow_phase_title(&title) {
        throw_type_error(scope, &error_text);
        return;
    }
    if scope
        .get_slot::<RuntimeState>()
        .is_some_and(|state| !state.active_workflow_nodes.is_empty())
    {
        throw_type_error(
            scope,
            "phase cannot change while workflow groups or agents are active",
        );
        return;
    }
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        if let Err(error_text) = state.admit_workflow_phase() {
            throw_type_error(scope, error_text);
            return;
        }
        let _ = state.event_tx.send(RuntimeEvent::Phase {
            title: title.clone(),
        });
        let Some(run_id) = state.run_id.clone() else {
            retval.set(v8::undefined(scope).into());
            return;
        };
        if let Some((phase_index, previous_title)) = state.active_workflow_phase.take() {
            state.emit_workflow_progress(WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
                run_id: run_id.clone(),
                phase_index,
                title: previous_title,
            }));
        }
        let phase_index = state.next_workflow_phase_index;
        state.next_workflow_phase_index = state.next_workflow_phase_index.saturating_add(1);
        state.active_workflow_phase = Some((phase_index, title.clone()));
        state.emit_workflow_progress(WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id,
            phase_index,
            title,
        }));
    }
    retval.set(v8::undefined(scope).into());
}

/// Begin a pure-JS `parallel`/`pipeline` group and return its topology ID. The orchestration
/// preludes capture this callback in a closure, then the raw helper global is deleted.
pub(in crate::runtime) fn workflow_group_begin_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(kind) = workflow_group_kind(scope, args.get(0)) else {
        return;
    };
    let Some(item_count) = workflow_u64_arg(scope, args.get(1), "workflow group item count") else {
        return;
    };
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    let run_id = state.run_id.clone();
    let group_id = match state.allocate_workflow_node_id() {
        Ok(group_id) => group_id,
        Err(error_text) => {
            throw_type_error(scope, error_text);
            return;
        }
    };
    state.active_workflow_nodes.insert(group_id);
    state.active_workflow_groups.insert(group_id);
    if let Some(run_id) = run_id {
        state.emit_workflow_progress(WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id,
            group_id,
            parent_node_id: state.active_workflow_parent_node_id,
            kind,
            item_count,
        }));
    }
    retval.set(v8::Number::new(scope, group_id as f64).into());
}

pub(in crate::runtime) fn workflow_group_end_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(group_id) = workflow_u64_arg(scope, args.get(0), "workflow group id") else {
        return;
    };
    let Some(kind) = workflow_group_kind(scope, args.get(1)) else {
        return;
    };
    let Some(item_count) = workflow_u64_arg(scope, args.get(2), "workflow group item count") else {
        return;
    };
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    if let Some(run_id) = state.run_id.clone() {
        state.emit_workflow_progress(WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id,
            group_id,
            kind,
            item_count,
        }));
    }
    state.active_workflow_nodes.remove(&group_id);
    state.active_workflow_groups.remove(&group_id);
    retval.set(v8::undefined(scope).into());
}

pub(in crate::runtime) fn workflow_group_enter_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let Some(group_id) = workflow_u64_arg(scope, args.get(0), "workflow group id") else {
        return;
    };
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    let previous = state.active_workflow_parent_node_id.replace(group_id);
    match previous {
        Some(previous) => retval.set(v8::Number::new(scope, previous as f64).into()),
        None => retval.set(v8::null(scope).into()),
    }
}

pub(in crate::runtime) fn workflow_group_exit_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let previous = args.get(0);
    let restored = if previous.is_null() || previous.is_undefined() {
        None
    } else {
        let Some(previous) = workflow_u64_arg(scope, previous, "workflow parent node id") else {
            return;
        };
        Some(previous)
    };
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        throw_type_error(scope, "runtime state unavailable");
        return;
    };
    state.active_workflow_parent_node_id = restored;
    retval.set(v8::undefined(scope).into());
}

fn workflow_group_kind(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<WorkflowGroupKind> {
    if !value.is_string() {
        throw_type_error(scope, "workflow group kind must be a string");
        return None;
    }
    match value.to_rust_string_lossy(scope).as_str() {
        "parallel" => Some(WorkflowGroupKind::Parallel),
        "pipeline" => Some(WorkflowGroupKind::Pipeline),
        _ => {
            throw_type_error(scope, "unknown workflow group kind");
            None
        }
    }
}

fn workflow_u64_arg(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    description: &str,
) -> Option<u64> {
    let Some(value) = value.number_value(scope) else {
        throw_type_error(scope, &format!("{description} must be a number"));
        return None;
    };
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u64::MAX as f64 {
        throw_type_error(
            scope,
            &format!("{description} must be a non-negative integer"),
        );
        return None;
    }
    Some(value as u64)
}
