use super::*;

pub(super) fn expected_completed_model() -> WorkflowRunModel {
    let plan_usage = token_usage(20);
    let execute_usage = token_usage(5);
    let total_usage = TokenUsage {
        input_tokens: 15,
        cached_input_tokens: 4,
        output_tokens: 10,
        reasoning_output_tokens: 6,
        total_tokens: 25,
    };
    WorkflowRunModel {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "release-audit".to_string(),
        args_digest: "blake3:args".to_string(),
        state: WorkflowRunState::Completed,
        status: AgentStatus::Completed(Some("done".to_string())),
        terminal_reason: Some(WorkflowRunTerminalReason::Completed),
        phases: vec![
            WorkflowPhase {
                index: 0,
                title: "plan".to_string(),
                state: WorkflowPhaseState::Completed,
                implicit: false,
                root_node_ids: vec![10],
                aggregate: WorkflowAggregate {
                    group_count: 2,
                    agent_count: 1,
                    completed_agent_count: 1,
                    token_usage: plan_usage.clone(),
                    tool_call_count: 2,
                    ..WorkflowAggregate::default()
                },
            },
            WorkflowPhase {
                index: 1,
                title: "execute".to_string(),
                state: WorkflowPhaseState::Completed,
                implicit: false,
                root_node_ids: vec![13],
                aggregate: WorkflowAggregate {
                    agent_count: 1,
                    completed_agent_count: 1,
                    returned_null_count: 1,
                    token_usage: execute_usage.clone(),
                    tool_call_count: 1,
                    ..WorkflowAggregate::default()
                },
            },
            WorkflowPhase {
                index: 2,
                title: "verify".to_string(),
                state: WorkflowPhaseState::Completed,
                implicit: false,
                root_node_ids: vec![14],
                aggregate: WorkflowAggregate {
                    group_count: 1,
                    ..WorkflowAggregate::default()
                },
            },
        ],
        topology: BTreeMap::from([
            (
                10,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 10,
                    parent_node_id: None,
                    phase_index: 0,
                    kind: WorkflowGroupKind::Parallel,
                    item_count: 2,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: vec![11, 12],
                }),
            ),
            (
                11,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 11,
                    parent_node_id: Some(10),
                    phase_index: 0,
                    kind: WorkflowGroupKind::Pipeline,
                    item_count: 0,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: Vec::new(),
                }),
            ),
            (
                12,
                WorkflowTopologyNode::Agent(WorkflowAgent {
                    id: 12,
                    attempt: 0,
                    last_attempt_reason: None,
                    parent_node_id: Some(10),
                    phase_index: 0,
                    label: "review-api".to_string(),
                    model: "gpt-5.4".to_string(),
                    effort: ReasoningEffort::High,
                    child_thread_id: Some("thread-12".to_string()),
                    state: WorkflowNodeState::Completed,
                    status: AgentStatus::Completed(Some("ok".to_string())),
                    token_usage: plan_usage,
                    tool_call_count: 2,
                    duration_ms: 0,
                    returned_null: false,
                    child_node_ids: Vec::new(),
                }),
            ),
            (
                13,
                WorkflowTopologyNode::Agent(WorkflowAgent {
                    id: 13,
                    attempt: 0,
                    last_attempt_reason: None,
                    parent_node_id: None,
                    phase_index: 1,
                    label: "run-tests".to_string(),
                    model: "gpt-5.4".to_string(),
                    effort: ReasoningEffort::High,
                    child_thread_id: Some("thread-13".to_string()),
                    state: WorkflowNodeState::Completed,
                    status: AgentStatus::Errored("boom".to_string()),
                    token_usage: execute_usage,
                    tool_call_count: 1,
                    duration_ms: 0,
                    returned_null: true,
                    child_node_ids: Vec::new(),
                }),
            ),
            (
                14,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 14,
                    parent_node_id: None,
                    phase_index: 2,
                    kind: WorkflowGroupKind::Parallel,
                    item_count: 0,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: Vec::new(),
                }),
            ),
        ]),
        topology_order: vec![10, 11, 12, 13, 14],
        aggregate: WorkflowAggregate {
            group_count: 3,
            agent_count: 2,
            completed_agent_count: 2,
            returned_null_count: 1,
            token_usage: total_usage,
            tool_call_count: 3,
            ..WorkflowAggregate::default()
        },
        budget: Some(WorkflowBudgetSummary {
            spent: 25,
            total: Some(1_000),
        }),
        active_phase_index: None,
        next_phase_index: 3,
    }
}
