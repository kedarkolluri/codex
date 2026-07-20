use codex_code_mode::CellId;
use codex_core_workflows::WorkflowBudget;
use codex_core_workflows::WorkflowBudgetLimit;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowEvent;
use pretty_assertions::assert_eq;

use super::WorkflowRunLedger;

/// The ledger records run→parent links and forgets only transient cell state.
#[test]
fn workflow_run_ledger_records_parent_linkage() {
    let ledger = WorkflowRunLedger::default();
    let parent_cell = CellId::new("1".to_string());
    let child_cell = CellId::new("2".to_string());

    ledger.register_run(
        parent_cell.clone(),
        "run-parent".to_string(),
        None,
        0,
        WorkflowBudget::new(WorkflowBudgetLimit::Unmetered),
        Some(0),
    );
    assert_eq!(
        ledger.parent_run_id_for_cell(&parent_cell),
        Some("run-parent".to_string())
    );
    assert_eq!(ledger.depth_for_cell(&parent_cell), Some(0));
    ledger.register_run(
        child_cell.clone(),
        "run-child".to_string(),
        ledger.parent_run_id_for_cell(&parent_cell),
        crate::agent::next_spawn_depth(ledger.depth_for_cell(&parent_cell).unwrap_or(0)),
        WorkflowBudget::new(WorkflowBudgetLimit::Unmetered),
        Some(0),
    );
    assert_eq!(ledger.depth_for_cell(&child_cell), Some(1));

    assert_eq!(
        ledger.links(),
        vec![
            super::WorkflowRunLink {
                run_id: "run-parent".to_string(),
                parent_run_id: None,
            },
            super::WorkflowRunLink {
                run_id: "run-child".to_string(),
                parent_run_id: Some("run-parent".to_string()),
            },
        ]
    );

    ledger.forget_cell(&parent_cell);
    assert_eq!(ledger.parent_run_id_for_cell(&parent_cell), None);
    assert_eq!(ledger.links().len(), 2);
}

#[test]
fn reserved_terminal_claim_drops_late_child_progress_and_owns_cleanup_facts() {
    let ledger = WorkflowRunLedger::default();
    let cell = CellId::new("pause-cell".to_string());
    ledger.register_run(
        cell.clone(),
        "run-pause".to_string(),
        None,
        0,
        WorkflowBudget::new(WorkflowBudgetLimit::Unmetered),
        None,
    );
    assert!(ledger.observe_progress(
        &cell,
        &WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: "run-pause".to_string(),
            node_id: 7,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: None,
            label: "active".to_string(),
            phase: None,
            model: "gpt-5".to_string(),
            effort: ReasoningEffort::Medium,
        })
    ));

    let facts = ledger
        .claim_terminal(&cell)
        .expect("pause reserves terminal facts");
    assert_eq!(facts.active_agents.len(), 1);
    assert_eq!(facts.active_agents[0].node_id, 7);
    assert_eq!(facts.active_agents[0].attempt, 0);
    assert_eq!(facts.active_agents[0].last_attempt_reason, None);
    assert!(!ledger.observe_progress(
        &cell,
        &WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: "run-pause".to_string(),
            node_id: 7,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Interrupted,
            token_usage: TokenUsage::default(),
            tool_call_count: 0,
            duration_ms: 0,
            returned_null: true,
        })
    ));
    assert!(ledger.claim_terminal(&cell).is_none());
}
