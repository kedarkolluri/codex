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
        // tool metadata); `workflow.runId` is the host-minted uuid v7. Both are
        // defined non-writable/non-deletable so the script can neither reassign
        // them nor derive ids/time/random itself (§4, §7).
        install_workflow_args_global(scope, global)?;
        install_workflow_object_global(scope, global)?;

        // Pure JS orchestration prelude. `parallel()` is a position-preserving
        // barrier defined entirely in the isolate (no host op) — it composes
        // `agent()` promises and inherits host-side concurrency bounding from the
        // scheduler semaphore (§4/§5). Gated on the same workflow flag as the
        // other narrator globals so it never leaks into plain code-mode exec.
        install_parallel_prelude(scope)?;
    }
    Ok(())
}

/// The injected JS workflow prelude. `parallel(thunks)` runs every thunk
/// concurrently and awaits them all (barrier), preserving input position: a
/// thunk that rejects resolves to `null` at its slot without failing siblings.
/// The `thunks.length <= 4096` item cap (§5) is validated *before* any thunk is
/// dispatched — `Array.prototype.map` only fires the thunks once the guard has
/// passed. Implemented purely as `Promise.all(thunks.map(t => t().catch(() =>
/// null)))`; there is no host op.
const PARALLEL_PRELUDE: &str = r#"
Object.defineProperty(globalThis, "parallel", {
  value: function parallel(thunks) {
    if (!Array.isArray(thunks)) {
      throw new TypeError(
        "parallel(thunks): expected an array of () => Promise thunks"
      );
    }
    if (thunks.length > 4096) {
      throw new RangeError(
        "parallel(thunks): item cap exceeded — " +
          thunks.length +
          " thunks requested but the maximum is 4096"
      );
    }
    return Promise.all(thunks.map((t) => t().catch(() => null)));
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

/// Install the read-only `workflow` object exposing the host-minted `runId`.
/// Both the object binding and its `runId` property are non-writable. An absent
/// run id installs `runId` as `null`.
fn install_workflow_object_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let run_id = scope
        .get_slot::<RuntimeState>()
        .and_then(|state| state.run_id.clone());
    let workflow = v8::Object::new(scope);
    let run_id_value: v8::Local<'s, v8::Value> = match run_id {
        Some(run_id) => v8::String::new(scope, &run_id)
            .ok_or_else(|| "failed to allocate workflow.runId".to_string())?
            .into(),
        None => v8::null(scope).into(),
    };
    define_readonly_property(scope, workflow, "runId", run_id_value)?;
    define_readonly_property(scope, global, "workflow", workflow.into())
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
