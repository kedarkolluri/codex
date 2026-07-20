use super::*;

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

    #[test]
    fn oversized_plain_return_resolves_to_null() {
        let out = finalize_agent_output(
            Some("x".repeat(codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES)),
            None,
        );
        assert_eq!(out, None);
    }

    #[test]
    fn oversized_structured_return_resolves_to_null_before_parsing() {
        let schema = json!({});
        let out = finalize_agent_output(
            Some(format!(
                "{{\"value\":\"{}\"}}",
                "x".repeat(codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES)
            )),
            Some(&schema),
        );
        assert_eq!(out, None);
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
mod budget_thread_goal_tests {
    use super::*;

    /// An UNMETERED run leaves the budget unconfigured (both getters read 0); there is no ceiling to
    /// report, so no ThreadGoal is produced (nothing is emitted for an unmetered workflow).
    #[test]
    fn no_thread_goal_for_unmetered_run() {
        let budget = WorkflowBudget::new(WorkflowBudgetLimit::Unmetered);
        assert!(build_budget_thread_goal(ThreadId::new(), &budget).is_none());
    }

    /// Below the ceiling a metered run reports `Active` carrying `token_budget = budget.total` and
    /// `tokens_used = budget.spent()` on the existing ThreadGoal channel.
    #[test]
    fn thread_goal_reports_active_below_ceiling() {
        let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(1_000));
        budget.record_spent(100);

        let thread_id = ThreadId::new();
        let goal =
            build_budget_thread_goal(thread_id, &budget).expect("metered run reports a goal");
        assert_eq!(goal.thread_id, thread_id);
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        assert_eq!(goal.token_budget, Some(1_000));
        assert_eq!(goal.tokens_used, 100);
    }

    /// At the ceiling (`remaining <= 0`) the run reports [`ThreadGoalStatus::BudgetLimited`] — the
    /// budget-limited condition surfaced through the existing ThreadGoal channel (no new protocol
    /// types), with `tokens_used` at the exhausted total.
    #[test]
    fn thread_goal_reports_budget_limited_at_ceiling() {
        let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(1_000));
        budget.record_spent(1_000);

        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("metered run reports a goal");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.token_budget, Some(1_000));
        assert_eq!(goal.tokens_used, 1_000);
    }

    /// Overshoot (an in-flight turn pushing spend past the limit) still reports `BudgetLimited`,
    /// and `token_budget` reports the CONFIGURED ceiling (1000), NOT the overshot spend (1500) —
    /// finding #9: an overshot 1000-limit with 1500 spent must report 1000, not 1500.
    #[test]
    fn thread_goal_budget_limited_on_overshoot() {
        let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(1_000));
        budget.record_spent(1_500);

        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("metered run reports a goal");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.tokens_used, 1_500);
        // token_budget is the configured ceiling, not spent+remaining.
        assert_eq!(goal.token_budget, Some(1_000));
    }

    /// An explicit zero ceiling is metered: it reports a goal with `token_budget: Some(0)` and
    /// `BudgetLimited` immediately (there is no headroom).
    #[test]
    fn thread_goal_reports_zero_ceiling_as_budget_limited() {
        let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(0));
        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("zero ceiling is metered");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.token_budget, Some(0));
        assert_eq!(goal.tokens_used, 0);
    }
}
