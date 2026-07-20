use super::*;

impl DurableProgressSnapshot {
    pub(super) fn seed(event: &WorkflowEvent) -> Self {
        Self {
            schema_version: PROGRESS_SCHEMA_VERSION,
            sequence: 0,
            run_id: event_run_id(event).to_string(),
            resumed_from_run_id: None,
            name: String::new(),
            args_digest: String::new(),
            state: DurableRunState::Running,
            status: DurableRunStatus::Running,
            phases: BTreeMap::new(),
            topology: BTreeMap::new(),
            budget: None,
            active_phase_index: None,
            begun: false,
        }
    }

    pub(super) fn reduce(&mut self, event: &WorkflowEvent) -> io::Result<()> {
        self.reduce_with_terminal_status(event, None)
    }

    pub(super) fn reduce_with_terminal_status(
        &mut self,
        event: &WorkflowEvent,
        terminal_status: Option<DurableRunStatus>,
    ) -> io::Result<()> {
        if event_run_id(event) != self.run_id {
            return Err(invalid_data(format!(
                "workflow progress run id mismatch: expected {}, got {}",
                self.run_id,
                event_run_id(event)
            )));
        }
        if self.state == DurableRunState::Terminal {
            if let WorkflowEvent::RunEnd(end) = event {
                let status = resolve_run_end_status(end, terminal_status)?;
                let duplicate = self.status == status
                    && self.budget
                        == Some(DurableBudget {
                            spent: end.spent,
                            total: end.total,
                        });
                return if duplicate {
                    Ok(())
                } else {
                    Err(invalid_data(
                        "workflow progress has conflicting terminal events",
                    ))
                };
            }
            return Err(invalid_data("workflow progress is already terminal"));
        }

        match event {
            WorkflowEvent::RunBegin(event) => self.apply_run_begin(event)?,
            WorkflowEvent::RunEnd(event) => {
                self.state = DurableRunState::Terminal;
                self.status = resolve_run_end_status(event, terminal_status)?;
                self.budget = Some(DurableBudget {
                    spent: event.spent,
                    total: event.total,
                });
                if let Some(index) = self.active_phase_index.take()
                    && let Some(phase) = self.phases.get_mut(&index)
                {
                    phase.state = DurablePhaseState::Completed;
                }
            }
            WorkflowEvent::PhaseBegin(event) => {
                let phase = self
                    .phases
                    .entry(event.phase_index)
                    .or_insert(DurablePhase {
                        index: event.phase_index,
                        title: event.title.clone(),
                        state: DurablePhaseState::Active,
                        implicit: false,
                        begun: true,
                    });
                if phase.begun && phase.title != event.title {
                    return Err(invalid_data(format!(
                        "workflow phase {} has conflicting titles",
                        event.phase_index
                    )));
                }
                phase.title.clone_from(&event.title);
                phase.begun = true;
                phase.implicit = false;
                if phase.state != DurablePhaseState::Completed {
                    phase.state = DurablePhaseState::Active;
                    self.active_phase_index = Some(event.phase_index);
                }
            }
            WorkflowEvent::PhaseEnd(event) => {
                let phase = self
                    .phases
                    .entry(event.phase_index)
                    .or_insert(DurablePhase {
                        index: event.phase_index,
                        title: event.title.clone(),
                        state: DurablePhaseState::Completed,
                        implicit: false,
                        begun: false,
                    });
                if phase.begun && phase.title != event.title {
                    return Err(invalid_data(format!(
                        "workflow phase {} has conflicting titles",
                        event.phase_index
                    )));
                }
                phase.title.clone_from(&event.title);
                phase.state = DurablePhaseState::Completed;
                if self.active_phase_index == Some(event.phase_index) {
                    self.active_phase_index = None;
                }
            }
            WorkflowEvent::GroupBegin(event) => self.apply_group_begin(event)?,
            WorkflowEvent::GroupEnd(event) => {
                let phase_index = self.current_phase_index();
                match self.topology.entry(event.group_id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(DurableNode::Group(DurableGroup {
                            id: event.group_id,
                            parent_node_id: None,
                            phase_index,
                            kind: event.kind,
                            item_count: event.item_count,
                            state: DurableNodeState::Completed,
                            begun: false,
                        }));
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        let DurableNode::Group(group) = entry.get_mut() else {
                            return Err(invalid_data(format!(
                                "workflow node {} changes kind",
                                event.group_id
                            )));
                        };
                        if group.kind != event.kind || group.item_count != event.item_count {
                            return Err(invalid_data(format!(
                                "workflow group {} has conflicting definitions",
                                event.group_id
                            )));
                        }
                        group.state = DurableNodeState::Completed;
                    }
                }
            }
            WorkflowEvent::AgentBegin(event) => self.apply_agent_begin(event)?,
            WorkflowEvent::AgentBound(event) => {
                uuid::Uuid::parse_str(&event.child_thread_id).map_err(|error| {
                    invalid_data(format!(
                        "workflow agent {} has invalid child thread id: {error}",
                        event.node_id
                    ))
                })?;
                let agent = self.agent_or_placeholder(
                    event.node_id,
                    event.attempt,
                    AttemptReasonCheck::Ignore,
                )?;
                if let Some(existing) = &agent.child_thread_id
                    && existing != &event.child_thread_id
                {
                    return Err(invalid_data(format!(
                        "workflow agent {} has conflicting child bindings",
                        event.node_id
                    )));
                }
                agent.child_thread_id = Some(event.child_thread_id.clone());
            }
            WorkflowEvent::AgentUpdated(event) => {
                let agent = self.agent_or_placeholder(
                    event.node_id,
                    event.attempt,
                    AttemptReasonCheck::Exact(event.last_attempt_reason),
                )?;
                merge_token_usage(&mut agent.token_usage, &event.token_usage);
                agent.tool_call_count = agent.tool_call_count.max(event.tool_call_count);
                agent.duration_ms = agent.duration_ms.max(event.duration_ms);
            }
            WorkflowEvent::AgentEnd(event) => self.apply_agent_end(event)?,
            WorkflowEvent::Log(_) => {}
        }
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    fn apply_run_begin(&mut self, event: &WorkflowRunBeginEvent) -> io::Result<()> {
        if self.begun {
            if self.name == event.name
                && self.args_digest == event.args_digest
                && self.resumed_from_run_id == event.resumed_from_run_id
            {
                return Ok(());
            }
            return Err(invalid_data("workflow run has conflicting begin events"));
        }
        if event.phases.len() > MAX_PROGRESS_PHASES {
            return Err(invalid_data(format!(
                "workflow run declares more than {MAX_PROGRESS_PHASES} phases"
            )));
        }
        self.name.clone_from(&event.name);
        self.resumed_from_run_id
            .clone_from(&event.resumed_from_run_id);
        self.args_digest.clone_from(&event.args_digest);
        self.begun = true;
        if event.phases.is_empty() {
            self.phases.entry(0).or_insert(DurablePhase {
                index: 0,
                title: "root".to_string(),
                state: DurablePhaseState::Active,
                implicit: true,
                begun: true,
            });
            self.active_phase_index.get_or_insert(0);
            return Ok(());
        }
        for (index, title) in event.phases.iter().enumerate() {
            let Ok(index) = u64::try_from(index) else {
                break;
            };
            self.phases.entry(index).or_insert(DurablePhase {
                index,
                title: title.clone(),
                state: DurablePhaseState::Pending,
                implicit: false,
                begun: false,
            });
        }
        Ok(())
    }

    fn apply_group_begin(&mut self, event: &WorkflowGroupBeginEvent) -> io::Result<()> {
        let phase_index = self.current_phase_index();
        match self.topology.entry(event.group_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(DurableNode::Group(DurableGroup {
                    id: event.group_id,
                    parent_node_id: event.parent_node_id,
                    phase_index,
                    kind: event.kind,
                    item_count: event.item_count,
                    state: DurableNodeState::Active,
                    begun: true,
                }));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let DurableNode::Group(group) = entry.get_mut() else {
                    return Err(invalid_data(format!(
                        "workflow node {} changes kind",
                        event.group_id
                    )));
                };
                if group.begun
                    && (group.parent_node_id != event.parent_node_id
                        || group.kind != event.kind
                        || group.item_count != event.item_count)
                {
                    return Err(invalid_data(format!(
                        "workflow group {} has conflicting definitions",
                        event.group_id
                    )));
                }
                group.parent_node_id = event.parent_node_id;
                group.phase_index = phase_index;
                group.kind = event.kind;
                group.item_count = event.item_count;
                group.begun = true;
            }
        }
        Ok(())
    }

    fn apply_agent_begin(&mut self, event: &WorkflowAgentBeginEvent) -> io::Result<()> {
        let phase_index = event
            .phase
            .as_ref()
            .and_then(|title| {
                self.phases
                    .values()
                    .find(|phase| &phase.title == title)
                    .map(|phase| phase.index)
            })
            .unwrap_or_else(|| self.current_phase_index());
        match self.topology.entry(event.node_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                if event.attempt != 0 || event.last_attempt_reason.is_some() {
                    return Err(invalid_data(format!(
                        "workflow agent {} must begin at attempt zero",
                        event.node_id
                    )));
                }
                entry.insert(DurableNode::Agent(DurableAgent {
                    id: event.node_id,
                    attempt: event.attempt,
                    last_attempt_reason: event.last_attempt_reason,
                    parent_node_id: event.parent_node_id,
                    phase_index,
                    label: Some(event.label.clone()),
                    model: Some(event.model.clone()),
                    effort: Some(event.effort.clone()),
                    child_thread_id: None,
                    state: DurableNodeState::Active,
                    status: AgentStatus::Running,
                    token_usage: TokenUsage::default(),
                    tool_call_count: 0,
                    duration_ms: 0,
                    returned_null: false,
                    begun: true,
                }));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let DurableNode::Agent(agent) = entry.get_mut() else {
                    return Err(invalid_data(format!(
                        "workflow node {} changes kind",
                        event.node_id
                    )));
                };
                if agent.begun
                    && (agent.parent_node_id != event.parent_node_id
                        || agent.phase_index != phase_index
                        || agent.label.as_deref() != Some(event.label.as_str())
                        || agent.model.as_deref() != Some(event.model.as_str())
                        || agent.effort.as_ref() != Some(&event.effort))
                {
                    return Err(invalid_data(format!(
                        "workflow agent {} has conflicting definitions",
                        event.node_id
                    )));
                }
                if !agent.begun {
                    if agent.attempt != event.attempt {
                        return Err(unexpected_agent_attempt(
                            event.node_id,
                            agent.attempt,
                            event.attempt,
                        ));
                    }
                    if agent.state == DurableNodeState::Active {
                        agent.last_attempt_reason = event.last_attempt_reason;
                    }
                } else if event.attempt == agent.attempt
                    && event.last_attempt_reason == agent.last_attempt_reason
                {
                    return Ok(());
                } else {
                    let expected = agent.attempt.saturating_add(1);
                    if agent.state == DurableNodeState::Completed
                        || event.attempt != expected
                        || event.last_attempt_reason != Some(WorkflowAgentAttemptReason::UserRetry)
                    {
                        return Err(unexpected_agent_attempt(
                            event.node_id,
                            expected,
                            event.attempt,
                        ));
                    }
                    agent.attempt = event.attempt;
                    agent.last_attempt_reason = event.last_attempt_reason;
                    agent.child_thread_id = None;
                    agent.state = DurableNodeState::Active;
                    agent.status = AgentStatus::Running;
                    agent.returned_null = false;
                }
                agent.parent_node_id = event.parent_node_id;
                agent.phase_index = phase_index;
                agent.label = Some(event.label.clone());
                agent.model = Some(event.model.clone());
                agent.effort = Some(event.effort.clone());
                agent.begun = true;
            }
        }
        Ok(())
    }

    fn apply_agent_end(&mut self, event: &WorkflowAgentEndEvent) -> io::Result<()> {
        if matches!(
            event.status,
            AgentStatus::PendingInit | AgentStatus::Running
        ) {
            return Err(invalid_data(format!(
                "workflow agent {} has non-terminal end status",
                event.node_id
            )));
        }
        let agent =
            self.agent_or_placeholder(event.node_id, event.attempt, AttemptReasonCheck::Ignore)?;
        merge_token_usage(&mut agent.token_usage, &event.token_usage);
        agent.tool_call_count = agent.tool_call_count.max(event.tool_call_count);
        agent.duration_ms = agent.duration_ms.max(event.duration_ms);
        agent.state = DurableNodeState::Completed;
        agent.status.clone_from(&event.status);
        agent.last_attempt_reason = event.last_attempt_reason;
        agent.returned_null = event.returned_null;
        Ok(())
    }

    fn agent_or_placeholder(
        &mut self,
        node_id: u64,
        attempt: u32,
        reason_check: AttemptReasonCheck,
    ) -> io::Result<&mut DurableAgent> {
        let phase_index = self.current_phase_index();
        let node = self.topology.entry(node_id).or_insert_with(|| {
            DurableNode::Agent(DurableAgent {
                id: node_id,
                attempt,
                last_attempt_reason: match reason_check {
                    AttemptReasonCheck::Ignore => None,
                    AttemptReasonCheck::Exact(reason) => reason,
                },
                parent_node_id: None,
                phase_index,
                label: None,
                model: None,
                effort: None,
                child_thread_id: None,
                state: DurableNodeState::Active,
                status: AgentStatus::Running,
                token_usage: TokenUsage::default(),
                tool_call_count: 0,
                duration_ms: 0,
                returned_null: false,
                begun: false,
            })
        });
        match node {
            DurableNode::Agent(agent) => {
                if agent.attempt != attempt {
                    return Err(unexpected_agent_attempt(node_id, agent.attempt, attempt));
                }
                if let AttemptReasonCheck::Exact(reason) = reason_check {
                    if agent.begun && agent.last_attempt_reason != reason {
                        return Err(invalid_data(format!(
                            "workflow agent {node_id} has a mismatched attempt reason"
                        )));
                    }
                    if !agent.begun && agent.state == DurableNodeState::Active {
                        agent.last_attempt_reason = reason;
                    }
                }
                Ok(agent)
            }
            DurableNode::Group(_) => Err(invalid_data(format!(
                "workflow node {node_id} changes kind"
            ))),
        }
    }

    fn current_phase_index(&mut self) -> u64 {
        if let Some(index) = self.active_phase_index {
            return index;
        }
        self.phases.entry(0).or_insert(DurablePhase {
            index: 0,
            title: "root".to_string(),
            state: DurablePhaseState::Active,
            implicit: true,
            begun: false,
        });
        0
    }
}

#[derive(Clone, Copy)]
enum AttemptReasonCheck {
    Ignore,
    Exact(Option<WorkflowAgentAttemptReason>),
}

fn unexpected_agent_attempt(node_id: u64, expected: u32, actual: u32) -> io::Error {
    invalid_data(format!(
        "workflow agent {node_id} expected attempt {expected}, got {actual}"
    ))
}

fn merge_token_usage(current: &mut TokenUsage, next: &TokenUsage) {
    current.input_tokens = current.input_tokens.max(next.input_tokens);
    current.cached_input_tokens = current.cached_input_tokens.max(next.cached_input_tokens);
    current.output_tokens = current.output_tokens.max(next.output_tokens);
    current.reasoning_output_tokens = current
        .reasoning_output_tokens
        .max(next.reasoning_output_tokens);
    current.total_tokens = current.total_tokens.max(next.total_tokens);
}
