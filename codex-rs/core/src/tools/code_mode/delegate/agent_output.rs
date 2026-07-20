use super::*;

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
pub(super) fn finalize_agent_output(
    final_text: Option<String>,
    schema: Option<&JsonValue>,
) -> Option<JsonValue> {
    let final_text = final_text?;
    let Some(schema) = schema else {
        let value = JsonValue::String(final_text);
        return codex_workflow_journal::ensure_workflow_agent_return(&value)
            .map_err(|err| warn!("workflow agent() return rejected: {err}"))
            .ok()
            .map(|()| value);
    };
    // Reject before parsing so a provider cannot make the host materialize an unbounded JSON tree.
    if final_text.len() > codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES {
        warn!(
            "workflow agent() structured return rejected: raw output exceeds the {}-byte replay cap",
            codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES
        );
        return None;
    }
    let parsed: JsonValue = serde_json::from_str(&final_text)
        .map_err(|err| warn!("workflow agent() structured output is not valid JSON: {err}"))
        .ok()?;
    // Belt-and-suspenders: compile the schema and re-validate the parsed instance even though the
    // child was asked for strict mode. An uncompilable schema is treated as a validation failure
    // (null) rather than trusting unvalidated model output.
    let validator = jsonschema::validator_for(schema)
        .map_err(|err| warn!("workflow agent() opts.schema is not a valid JSON Schema: {err}"))
        .ok()?;
    if !validator.is_valid(&parsed) {
        warn!("workflow agent() structured output failed JSON Schema validation");
        return None;
    }
    codex_workflow_journal::ensure_workflow_agent_return(&parsed)
        .map_err(|err| warn!("workflow agent() structured return rejected: {err}"))
        .ok()
        .map(|()| parsed)
}

/// Objective carried on the workflow budget-reporting [`ThreadGoal`]. The ThreadGoal channel requires
/// a non-empty objective; a workflow run reports budget *state* (not a user-authored goal), so a
/// fixed, non-empty label is used.
pub(super) const WORKFLOW_BUDGET_GOAL_OBJECTIVE: &str = "workflow budget";

/// Emit the workflow budget-reporting [`ThreadGoal`] on the EXISTING ThreadGoal channel (spec §8),
/// carrying the current turn id so the update is scoped to the workflow turn (not a `turn_id: None`
/// session-global overwrite). Emits nothing for an unmetered run.
///
/// NOTE (finding #9, partial): this reuses the user-facing ThreadGoal channel as the §8 reporting
/// surface, so a budget update still visually replaces the client's rendered goal for the thread.
/// Fully scoping/restoring the user's authored goal needs session goal-state plumbing outside this
/// module; the in-scope hardening here is (a) reporting the CONFIGURED ceiling as `token_budget`
/// (not `spent + remaining`), (b) tagging the real `turn_id`, and (c) emitting on the spend crossing
/// so a final over-ceiling child still surfaces `BudgetLimited`.
pub(super) async fn emit_budget_thread_goal(
    session: &crate::session::session::Session,
    turn_sub_id: &str,
    budget: &WorkflowBudget,
) {
    let Some(goal) = build_budget_thread_goal(session.thread_id(), budget) else {
        return;
    };
    let event = Event {
        id: turn_sub_id.to_string(),
        msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
            thread_id: goal.thread_id,
            turn_id: Some(turn_sub_id.to_string()),
            goal,
        }),
    };
    session.send_event_raw(event).await;
}

/// Build the budget-reporting [`ThreadGoal`] from the run-local
/// [`WorkflowBudget`], reusing the existing ThreadGoal channel. Returns `None`
/// when the run and its ancestors are unmetered.
///
/// For a metered run: `token_budget` is the CONFIGURED ceiling (`budget.limit()`), NOT
/// `spent + remaining` — so an overshot 1000-token ceiling with 1500 spent still reports the 1000
/// limit rather than 1500 (finding #9). `tokens_used` is `budget.spent()`, and the status is
/// [`ThreadGoalStatus::BudgetLimited`] once the ceiling is reached (`remaining <= 0`), else
/// [`ThreadGoalStatus::Active`]. Pure and deterministic — the timestamp fields are zeroed so nothing
/// on the workflow path reads a wall clock (no `Date`).
pub(super) fn build_budget_thread_goal(
    thread_id: ThreadId,
    budget: &WorkflowBudget,
) -> Option<ThreadGoal> {
    let snapshot = budget.effective_snapshot();
    let WorkflowBudgetLimit::Limited(limit) = snapshot.limit else {
        return None;
    };
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let spent = i64::try_from(snapshot.spent).unwrap_or(i64::MAX);
    let status = if snapshot.remaining == Some(0) {
        ThreadGoalStatus::BudgetLimited
    } else {
        ThreadGoalStatus::Active
    };
    Some(ThreadGoal {
        thread_id,
        objective: WORKFLOW_BUDGET_GOAL_OBJECTIVE.to_string(),
        status,
        token_budget: Some(limit),
        tokens_used: spent,
        time_used_seconds: 0,
        created_at: 0,
        updated_at: 0,
    })
}
