//! Async-local workflow group attribution for V8 promise reactions.

use super::RuntimeState;

const PROMISE_GROUP_KEY: &str = "codex.workflow.group";

/// Propagate the group active when a promise is created into every later reaction. This makes an
/// `agent()` issued after one or more `await`s retain the correct parallel/pipeline parent even when
/// sibling async chains overlap.
pub(super) unsafe extern "C" fn promise_hook(
    hook_type: v8::PromiseHookType,
    promise: v8::Local<v8::Promise>,
    parent: v8::Local<v8::Value>,
) {
    v8::callback_scope!(unsafe scope, promise);
    let Some(context) = promise.get_creation_context(scope) else {
        return;
    };
    let scope = &mut v8::ContextScope::new(scope, context);
    match hook_type {
        v8::PromiseHookType::Init => {
            let inherited = v8::Local::<v8::Promise>::try_from(parent)
                .ok()
                .and_then(|parent| promise_group(scope, parent))
                .or_else(|| {
                    scope
                        .get_slot::<RuntimeState>()
                        .and_then(|state| state.active_workflow_parent_node_id)
                });
            if let Some(group_id) = inherited {
                set_promise_group(scope, promise, group_id);
            }
        }
        v8::PromiseHookType::Before => {
            let group_id = promise_group(scope, promise);
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                state
                    .workflow_parent_context_stack
                    .push(state.active_workflow_parent_node_id);
                state.active_workflow_parent_node_id = group_id;
            }
        }
        v8::PromiseHookType::After => {
            if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                state.active_workflow_parent_node_id =
                    state.workflow_parent_context_stack.pop().flatten();
            }
        }
        v8::PromiseHookType::Resolve => {}
    }
}

fn promise_group(
    scope: &mut v8::PinScope<'_, '_>,
    promise: v8::Local<'_, v8::Promise>,
) -> Option<u64> {
    let name = v8::String::new(scope, PROMISE_GROUP_KEY)?;
    let key = v8::Private::for_api(scope, Some(name));
    let value = promise.get_private(scope, key)?.number_value(scope)?;
    (value.is_finite() && value >= 0.0 && value.fract() == 0.0).then_some(value as u64)
}

fn set_promise_group(
    scope: &mut v8::PinScope<'_, '_>,
    promise: v8::Local<'_, v8::Promise>,
    group_id: u64,
) {
    let Some(name) = v8::String::new(scope, PROMISE_GROUP_KEY) else {
        return;
    };
    let key = v8::Private::for_api(scope, Some(name));
    let value = v8::Number::new(scope, group_id as f64);
    let _ = promise.set_private(scope, key, value.into());
}
