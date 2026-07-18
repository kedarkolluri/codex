use super::RuntimeState;
use super::callbacks::agent_callback;
use super::callbacks::clear_timeout_callback;
use super::callbacks::exit_callback;
use super::callbacks::generated_image_callback;
use super::callbacks::image_callback;
use super::callbacks::load_callback;
use super::callbacks::log_callback;
use super::callbacks::notify_callback;
use super::callbacks::phase_callback;
use super::callbacks::set_timeout_callback;
use super::callbacks::store_callback;
use super::callbacks::text_callback;
use super::callbacks::tool_callback;
use super::callbacks::workflow_callback;
use super::callbacks::yield_control_callback;
use super::value::json_to_v8;
use super::value::value_to_error_text;

pub(super) fn install_globals(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    delete_global(scope, global, "console")?;
    delete_global(scope, global, "Atomics")?;
    delete_global(scope, global, "SharedArrayBuffer")?;
    delete_global(scope, global, "WebAssembly")?;

    let tools = build_tools_object(scope)?;
    let all_tools = build_all_tools_value(scope)?;
    let clear_timeout = helper_function(scope, "clearTimeout", clear_timeout_callback)?;
    let set_timeout = helper_function(scope, "setTimeout", set_timeout_callback)?;
    let text = helper_function(scope, "text", text_callback)?;
    let image = helper_function(scope, "image", image_callback)?;
    let generated_image = helper_function(scope, "generatedImage", generated_image_callback)?;
    let store = helper_function(scope, "store", store_callback)?;
    let load = helper_function(scope, "load", load_callback)?;
    let notify = helper_function(scope, "notify", notify_callback)?;
    let yield_control = helper_function(scope, "yield_control", yield_control_callback)?;
    let exit = helper_function(scope, "exit", exit_callback)?;

    set_global(scope, global, "tools", tools.into())?;
    set_global(scope, global, "ALL_TOOLS", all_tools)?;
    set_global(scope, global, "clearTimeout", clear_timeout.into())?;
    set_global(scope, global, "setTimeout", set_timeout.into())?;
    set_global(scope, global, "text", text.into())?;
    set_global(scope, global, "image", image.into())?;
    set_global(scope, global, "generatedImage", generated_image.into())?;
    set_global(scope, global, "store", store.into())?;
    set_global(scope, global, "load", load.into())?;
    set_global(scope, global, "notify", notify.into())?;
    set_global(scope, global, "yield_control", yield_control.into())?;
    set_global(scope, global, "exit", exit.into())?;

    // Workflow-only narrator/grouping globals. These are gated on the cell being
    // a workflow run so `phase`/`log` never leak into plain code-mode exec
    // sessions (which have no `meta` manifest and must not see these names).
    let workflow = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.workflow)
        .unwrap_or(false);
    if workflow {
        let phase = helper_function(scope, "phase", phase_callback)?;
        let log = helper_function(scope, "log", log_callback)?;
        let agent = helper_function(scope, "agent", agent_callback)?;
        set_global(scope, global, "phase", phase.into())?;
        set_global(scope, global, "log", log.into())?;
        set_global(scope, global, "agent", agent.into())?;

        // Read-only host->isolate data globals. `args` is the invocation JSON
        // (injected via `json_to_v8`, exactly like `build_tools_object` injects
        // tool metadata). `workflow` is the callable nested-run hook
        // `workflow(nameOrRef, args)` (§4) that also carries the host-minted
        // `runId` uuid v7 as a read-only property (§7 `workflow.runId`). `args`
        // and `runId` are defined non-writable/non-deletable so the script can
        // neither reassign them nor derive ids/time/random itself (§4, §7).
        install_workflow_args_global(scope, global)?;
        install_workflow_object_global(scope, global)?;

        // Read-only native-backed `budget` global (§4/§8). `budget.total` is a
        // read-only number sourced from `args.budget.total`; `budget.spent()` and
        // `budget.remaining()` are native functions forwarding to the shared
        // `RolloutBudget` handle on the `RuntimeState`, read live at call time so
        // a workflow that awaits subagents sees updated spend.
        install_workflow_budget_global(scope, global)?;

        // Pure JS orchestration prelude. `parallel()` is a position-preserving
        // barrier defined entirely in the isolate (no host op) — it composes
        // `agent()` promises and inherits host-side concurrency bounding from the
        // scheduler semaphore (§4/§5). Gated on the same workflow flag as the
        // other narrator globals so it never leaks into plain code-mode exec.
        install_parallel_prelude(scope)?;

        // `pipeline()` is the no-barrier sibling of `parallel()`: each item runs
        // its own independent stage chain, so item A may be in stage 3 while item
        // B is still in stage 1 (§4/§5). Also pure JS with no host op — it
        // composes `agent()` promises exactly like `parallel()` and inherits the
        // same host-side scheduler-semaphore concurrency bound.
        install_pipeline_prelude(scope)?;
    }
    Ok(())
}

/// The injected JS workflow prelude. `parallel(thunks)` runs every thunk
/// concurrently and awaits them all (barrier), preserving input position: a
/// thunk that rejects resolves to `null` at its slot without failing siblings.
/// The `thunks.length <= 4096` item cap (§5) is validated *before* any thunk is
/// dispatched.
///
/// Proxy-safe by construction: `thunks.length` is read EXACTLY ONCE into `count`,
/// the cap is checked against that captured value, and precisely `count`
/// positions are copied into a fresh ordinary array (`snapshot`) that alone drives
/// dispatch. `Array.isArray` returns `true` for a `Proxy` wrapping an array, so a
/// length-lying proxy could otherwise report a small length to the guard and a
/// larger one when `Array.prototype.map` re-reads it — dispatching for >4096
/// positions. Because both the guard and the copy loop use the single captured
/// `count` (and `map` runs over the real `snapshot`, never the proxy), no more
/// than `count` (<= 4096) thunks can ever be invoked. There is no host op.
const PARALLEL_PRELUDE: &str = r#"
Object.defineProperty(globalThis, "parallel", {
  value: function parallel(thunks) {
    if (!Array.isArray(thunks)) {
      throw new TypeError(
        "parallel(thunks): expected an array of () => Promise thunks"
      );
    }
    const count = thunks.length;
    if (count > 4096) {
      throw new RangeError(
        "parallel(thunks): item cap exceeded — " +
          count +
          " thunks requested but the maximum is 4096"
      );
    }
    const snapshot = [];
    for (let i = 0; i < count; i++) {
      snapshot.push(thunks[i]);
    }
    return Promise.all(snapshot.map((t) => t().catch(() => null)));
  },
  writable: false,
  enumerable: false,
  configurable: false,
});
"#;

/// Compile and run the pure-JS [`PARALLEL_PRELUDE`] as a classic script so its
/// `parallel` binding is visible to the workflow module evaluated afterward.
fn install_parallel_prelude(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let source = v8::String::new(&tc, PARALLEL_PRELUDE)
        .ok_or_else(|| "failed to allocate parallel prelude source".to_string())?;
    let script = v8::Script::compile(&tc, source, None).ok_or_else(|| {
        tc.exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "failed to compile parallel prelude".to_string())
    })?;
    if script.run(&tc).is_none() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "failed to install parallel prelude".to_string()));
    }
    Ok(())
}

/// The injected JS `pipeline(items, ...stages)` prelude. Unlike `parallel()`
/// there is NO barrier between stages: every item is its own promise chain
/// (`stages.reduce((p, s) => p.then(s), Promise.resolve(item))`), so item A can
/// reach stage 3 while item B is still in stage 1 — the staggered progress falls
/// out of the shared host scheduler semaphore, which bounds *total* concurrent
/// agents rather than per-stage width. A stage that throws is caught per-item
/// (`.catch(() => null)`), dropping THAT item to `null` at its slot without
/// blocking its siblings, so the result stays position-preserving with length
/// `items.length`. The `items.length <= 4096` item cap (§5) is validated *before*
/// any stage runs.
///
/// Proxy-safe by construction, exactly like [`PARALLEL_PRELUDE`]: `items.length`
/// is read EXACTLY ONCE into `count`, the cap is checked against that captured
/// value, and precisely `count` positions are copied into a fresh ordinary array
/// (`snapshot`) that alone drives dispatch. A length-lying `Proxy` (which passes
/// `Array.isArray` when it wraps an array) therefore cannot report a small length
/// to the guard and a larger one when the map re-reads it: both the guard and the
/// copy loop use the single captured `count`, and `map` runs over the real
/// `snapshot`, so no more than `count` (<= 4096) stage chains are ever dispatched.
/// Pure JS with no host op; it composes `agent()` promises exactly like
/// `parallel()`.
const PIPELINE_PRELUDE: &str = r#"
Object.defineProperty(globalThis, "pipeline", {
  value: function pipeline(items, ...stages) {
    if (!Array.isArray(items)) {
      throw new TypeError(
        "pipeline(items, ...stages): expected an array of items"
      );
    }
    const count = items.length;
    if (count > 4096) {
      throw new RangeError(
        "pipeline(items, ...stages): item cap exceeded — " +
          count +
          " items requested but the maximum is 4096"
      );
    }
    const snapshot = [];
    for (let i = 0; i < count; i++) {
      snapshot.push(items[i]);
    }
    return Promise.all(
      snapshot.map((item) =>
        stages
          .reduce((p, s) => p.then(s), Promise.resolve(item))
          .catch(() => null)
      )
    );
  },
  writable: false,
  enumerable: false,
  configurable: false,
});
"#;

/// Compile and run the pure-JS [`PIPELINE_PRELUDE`] as a classic script so its
/// `pipeline` binding is visible to the workflow module evaluated afterward.
fn install_pipeline_prelude(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let source = v8::String::new(&tc, PIPELINE_PRELUDE)
        .ok_or_else(|| "failed to allocate pipeline prelude source".to_string())?;
    let script = v8::Script::compile(&tc, source, None).ok_or_else(|| {
        tc.exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "failed to compile pipeline prelude".to_string())
    })?;
    if script.run(&tc).is_none() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "failed to install pipeline prelude".to_string()));
    }
    Ok(())
}

/// Install the read-only `args` global from the invocation JSON carried on the
/// [`RuntimeState`]. Absent args install as `null`.
fn install_workflow_args_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let args = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.args.clone());
    let value: v8::Local<'s, v8::Value> = match args {
        Some(args) => json_to_v8(scope, &args)
            .ok_or_else(|| "failed to convert workflow args to a JS value".to_string())?,
        None => v8::null(scope).into(),
    };
    define_readonly_property(scope, global, "args", value)
}

/// Install the `workflow` global. In JS a function is also an object, so a single
/// `workflow` binding serves both roles: it is CALLABLE as
/// `workflow(nameOrRef, args)` (§4 nested run — routed through
/// [`workflow_callback`]) and it carries the host-minted `runId` as a read-only
/// own-property (§7 `workflow.runId`). Both the `workflow` binding and its
/// `runId` property are non-writable/non-deletable so a script can neither
/// reassign nor delete them. An absent run id installs `runId` as `null`.
fn install_workflow_object_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let run_id = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.run_id.clone());
    let workflow = helper_function(scope, "workflow", workflow_callback)?;
    let run_id_value: v8::Local<'s, v8::Value> = match run_id {
        Some(run_id) => v8::String::new(scope, &run_id)
            .ok_or_else(|| "failed to allocate workflow.runId".to_string())?
            .into(),
        None => v8::null(scope).into(),
    };
    define_readonly_property(scope, workflow.into(), "runId", run_id_value)?;
    define_readonly_property(scope, global, "workflow", workflow.into())
}

/// Install the native-backed read-only `budget` global (§4 `budget`; §8). The
/// object exposes `total` (a read-only number sourced from `args.budget.total`)
/// plus the native functions `spent()` and `remaining()`, which forward to the
/// shared [`WorkflowBudgetHandle`] on the [`RuntimeState`] and are therefore read
/// live at call time — a workflow that awaits subagents and re-reads
/// `budget.spent()` observes the updated tree-wide spend. The `budget` binding
/// and its `total` property are non-writable/non-deletable so the script can
/// neither reassign nor delete them. When no budget handle is threaded (plain
/// runs), `spent()` reports `0` and `remaining()` reports `total`.
fn install_workflow_budget_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let total = workflow_budget_ceiling(scope);
    let budget = v8::Object::new(scope);
    let total_value: v8::Local<'s, v8::Value> = v8::Number::new(scope, total as f64).into();
    define_readonly_property(scope, budget, "total", total_value)?;

    let spent = helper_function(scope, "spent", budget_spent_callback)?;
    let remaining = helper_function(scope, "remaining", budget_remaining_callback)?;
    define_readonly_property(scope, budget, "spent", spent.into())?;
    define_readonly_property(scope, budget, "remaining", remaining.into())?;

    define_readonly_property(scope, global, "budget", budget.into())
}

/// The workflow `budget.total` ceiling. Prefer the live budget handle's
/// configured limit (which equals `args.budget.total`, since the handler sets
/// `limit_tokens = args.budget.total`), falling back to reading
/// `args.budget.total` directly when no handle is threaded.
fn workflow_budget_ceiling(scope: &mut v8::PinScope<'_, '_>) -> i64 {
    let handle = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.budget.clone());
    match handle.as_ref() {
        Some(budget) => budget.total(),
        None => workflow_budget_total_from_args(scope),
    }
}

/// Read `args.budget.total` from the [`RuntimeState`]. An absent, non-object, or
/// non-integer value yields `0` so the global always installs a numeric `total`.
fn workflow_budget_total_from_args(scope: &mut v8::PinScope<'_, '_>) -> i64 {
    scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.args.as_ref())
        .and_then(|args| args.get("budget"))
        .and_then(|budget| budget.get("total"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
}

/// Native `budget.spent()` — returns the live weighted output-token spend from
/// the shared budget handle, or `0` when no handle is threaded.
fn budget_spent_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let handle = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.budget.clone());
    let spent = handle.as_ref().map(|budget| budget.spent()).unwrap_or(0);
    retval.set(v8::Number::new(scope, spent as f64).into());
}

/// Native `budget.remaining()` — returns the live remaining budget (clamped at
/// `0`) from the shared budget handle, or `budget.total` when no handle is
/// threaded.
fn budget_remaining_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let handle = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.budget.clone());
    let remaining = match handle.as_ref() {
        Some(budget) => budget.remaining(),
        None => workflow_budget_total_from_args(scope),
    };
    retval.set(v8::Number::new(scope, remaining as f64).into());
}

/// Define `name` on `object` as a non-writable, non-deletable data property so a
/// workflow script can neither reassign nor delete it. In the ES-module (strict)
/// runtime an assignment throws a `TypeError`; otherwise it is silently ignored.
fn define_readonly_property<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate read-only global `{name}`"))?;
    let attr = v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE;
    if object.define_own_property(scope, key.into(), value, attr) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to define read-only global `{name}`"))
    }
}

fn build_tools_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let tools = v8::Object::new(scope);
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.enabled_tools.clone())
        .unwrap_or_default();

    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate tool name".to_string())?;
        let function = tool_function(scope, tool_index)?;
        tools.set(scope, name.into(), function.into());
    }
    Ok(tools)
}

fn build_all_tools_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.enabled_tools.clone())
        .unwrap_or_default();
    let array = v8::Array::new(scope, enabled_tools.len() as i32);
    let name_key = v8::String::new(scope, "name")
        .ok_or_else(|| "failed to allocate ALL_TOOLS name key".to_string())?;
    let description_key = v8::String::new(scope, "description")
        .ok_or_else(|| "failed to allocate ALL_TOOLS description key".to_string())?;

    for (index, tool) in enabled_tools.iter().enumerate() {
        let item = v8::Object::new(scope);
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate ALL_TOOLS name".to_string())?;
        let description = v8::String::new(scope, &tool.description)
            .ok_or_else(|| "failed to allocate ALL_TOOLS description".to_string())?;

        if item.set(scope, name_key.into(), name.into()) != Some(true) {
            return Err("failed to set ALL_TOOLS name".to_string());
        }
        if item.set(scope, description_key.into(), description.into()) != Some(true) {
            return Err("failed to set ALL_TOOLS description".to_string());
        }
        if array.set_index(scope, index as u32, item.into()) != Some(true) {
            return Err("failed to append ALL_TOOLS metadata".to_string());
        }
    }

    Ok(array.into())
}

fn helper_function<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    callback: F,
) -> Result<v8::Local<'s, v8::Function>, String>
where
    F: v8::MapFnTo<v8::FunctionCallback>,
{
    let name =
        v8::String::new(scope, name).ok_or_else(|| "failed to allocate helper name".to_string())?;
    let template = v8::FunctionTemplate::builder(callback)
        .data(name.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create helper function".to_string())
}

fn tool_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tool_index: usize,
) -> Result<v8::Local<'s, v8::Function>, String> {
    let data = v8::String::new(scope, &tool_index.to_string())
        .ok_or_else(|| "failed to allocate tool callback data".to_string())?;
    let template = v8::FunctionTemplate::builder(tool_callback)
        .data(data.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create tool function".to_string())
}

fn set_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.set(scope, key.into(), value) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to set global `{name}`"))
    }
}

fn delete_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.delete(scope, key.into()) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to remove global `{name}`"))
    }
}
