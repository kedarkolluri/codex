//! Config-build overrides for the workflow `agent()` spawn-and-await helper.
//!
//! A workflow body can pass `opts.model` / `opts.effort` / `opts.agentType` to `agent(prompt, opts)`
//! (spec §4/§6). The spawn-and-await keystone
//! ([`crate::agent::control::AgentControl::spawn_and_await_final_message`]) threads those requested
//! values here and applies them to the freshly-built child config **before** spawning, in the same
//! order the V2 `spawn_agent` tool applies them (`tools/handlers/multi_agents_v2/spawn.rs`): the
//! model/effort overrides first, then the role layer. Omitted values inherit the parent turn config.

use crate::agent::role::apply_workflow_role_to_config;
use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents::apply_requested_spawn_agent_model_overrides;
use codex_protocol::openai_models::ReasoningEffort;

/// Requested `agent()` `opts.model` / `opts.effort` / `opts.agentType` overrides, threaded from the
/// workflow host into [`crate::agent::control::AgentControl::spawn_and_await_final_message`].
///
/// This is a **dedicated request struct** rather than fields on `SpawnAgentOptions`: these are all
/// *config-build* inputs (applied to the child `Config` before the spawn), whereas
/// `SpawnAgentOptions` carries spawn *mechanics* (fork mode, environments, parent id) consumed by the
/// generic `spawn_agent_deferred_input` path — which never reads these overrides. Keeping them apart
/// avoids adding workflow-only knobs that every non-workflow spawn path would silently ignore.
///
/// The values are carried as the raw workflow strings (the JS boundary hands over strings); the
/// `low..max` -> [`ReasoningEffort`] mapping, per-model validation, and role resolution happen in
/// [`Self::apply`].
#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnAgentConfigOverrides {
    /// `opts.model`: a model slug resolved against the session `ModelsManager`. `None` inherits the
    /// parent turn's model.
    pub(crate) model: Option<String>,
    /// `opts.effort`: one of `low|medium|high|xhigh|max` (spec §4). `None` inherits the parent turn's
    /// reasoning effort (or the resolved model's default when a new model is selected).
    pub(crate) effort: Option<String>,
    /// `opts.agentType`: a role name resolved via [`apply_workflow_role_to_config`] (spec §6 step
    /// 2). `None`
    /// (or a blank/whitespace-only string, mirroring the V2 `spawn_agent` tool) falls back to
    /// [`DEFAULT_ROLE_NAME`], whose role layer is a no-op that leaves the inherited config untouched.
    pub(crate) agent_type: Option<String>,
}

impl SpawnAgentConfigOverrides {
    /// True when no override was requested, so the config-build step can skip resolving the session
    /// `ModelsManager` entirely and leave the parent-inherited config untouched.
    ///
    /// A blank/whitespace-only `agent_type` is treated as absent (it resolves to the no-op default
    /// role), so it does not by itself make the override non-empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.model.is_none() && self.effort.is_none() && self.role_name().is_none()
    }

    /// The trimmed, non-empty `opts.agentType` role name, or `None` when absent/blank.
    ///
    /// Mirrors the V2 `spawn_agent` tool (`multi_agents_v2/spawn.rs`), which trims `agent_type` and
    /// treats an empty string as "no role requested" so it falls back to [`DEFAULT_ROLE_NAME`].
    fn role_name(&self) -> Option<&str> {
        self.agent_type
            .as_deref()
            .map(str::trim)
            .filter(|role| !role.is_empty())
    }

    /// Apply the requested `opts.model` / `opts.effort` / `opts.agentType` onto `config`.
    ///
    /// Applied in the same order as the V2 `spawn_agent` tool (`multi_agents_v2/spawn.rs`): the
    /// model/effort overrides first, then the role layer last (so a role that locks a model/effort
    /// takes precedence over a requested one, matching the "these settings cannot be changed" role
    /// contract).
    ///
    /// `opts.model` resolves against the session `ModelsManager` and, when present, `opts.effort` is
    /// validated against the *resolved* model's `supported_reasoning_levels`; when only `opts.effort`
    /// is present it is validated against the parent turn's current model. `opts.agentType` resolves
    /// to a role via [`apply_workflow_role_to_config`], falling back to [`DEFAULT_ROLE_NAME`] when
    /// absent. Any
    /// unresolved model, unsupported effort, or unknown/unavailable role surfaces an actionable
    /// [`FunctionCallError`] rather than silently spawning with the wrong settings or a silent default
    /// role. Delegates to [`apply_requested_spawn_agent_model_overrides`] and
    /// [`apply_workflow_role_to_config`] so workflow and ordinary roles share loading semantics
    /// while the workflow path adds its tighter context boundary.
    pub(crate) async fn apply(
        &self,
        session: &Session,
        parent_turn: &TurnContext,
        config: &mut Config,
    ) -> Result<(), FunctionCallError> {
        let requested_effort = self
            .effort
            .as_deref()
            .map(map_workflow_effort)
            .transpose()?;
        apply_requested_spawn_agent_model_overrides(
            session,
            parent_turn,
            config,
            self.model.as_deref(),
            requested_effort,
        )
        .await?;
        // Role layer last, matching V2 `spawn_agent` ordering (`multi_agents_v2/spawn.rs`). An
        // absent/blank `agent_type` resolves to `DEFAULT_ROLE_NAME`; an unknown role is an error, not
        // a silent default.
        self.apply_role_layer(config).await
    }

    /// Apply only the role layer (`opts.agentType`, defaulting to [`DEFAULT_ROLE_NAME`]) to `config`,
    /// without touching the model/effort overrides or needing the session `ModelsManager`.
    ///
    /// This is the step that must run **even when no override was requested** ([`Self::is_empty`]):
    /// the V2 `spawn_agent` tool always calls `apply_role_to_config(.., None)` on its non-fork path,
    /// so a user-defined role literally named `default` ([`DEFAULT_ROLE_NAME`]) is applied to a bare
    /// `agent("prompt")`. Skipping it for the empty-override case would silently drop that role.
    pub(crate) async fn apply_role_layer(
        &self,
        config: &mut Config,
    ) -> Result<(), FunctionCallError> {
        apply_workflow_role_to_config(config, self.role_name())
            .await
            .map_err(FunctionCallError::RespondToModel)
    }
}

/// Map a workflow `agent()` `opts.effort` string onto Codex's [`ReasoningEffort`].
///
/// The workflow authoring API restricts `opts.effort` to `low|medium|high|xhigh|max` (spec §4), so
/// any other value (`none`, `minimal`, `ultra`, the empty string, or a typo) is rejected with an
/// actionable error instead of being forwarded as an out-of-contract effort. Whether the mapped
/// effort is actually supported by the selected model is a separate check performed downstream by
/// `validate_spawn_agent_reasoning_effort`.
pub(crate) fn map_workflow_effort(effort: &str) -> Result<ReasoningEffort, FunctionCallError> {
    match effort {
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::XHigh),
        "max" => Ok(ReasoningEffort::Max),
        other => Err(FunctionCallError::RespondToModel(format!(
            "Unsupported workflow reasoning effort `{other}`; expected one of: low, medium, high, xhigh, max"
        ))),
    }
}
