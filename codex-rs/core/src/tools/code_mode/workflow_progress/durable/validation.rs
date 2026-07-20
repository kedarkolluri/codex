use super::*;

impl DurableProgressSnapshot {
    pub(super) fn validate(&self, expected_run_id: &str) -> io::Result<()> {
        if self.schema_version != PROGRESS_SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported workflow progress schema version {}",
                self.schema_version
            )));
        }
        if self.run_id != expected_run_id {
            return Err(invalid_data(format!(
                "workflow progress run id `{}` does not match requested run `{expected_run_id}`",
                self.run_id
            )));
        }
        uuid::Uuid::parse_str(&self.run_id)
            .map_err(|error| invalid_data(format!("invalid workflow progress run id: {error}")))?;
        if self.phases.len() > MAX_PROGRESS_PHASES {
            return Err(invalid_data(format!(
                "workflow progress exceeds the {MAX_PROGRESS_PHASES}-phase cap"
            )));
        }
        if self.topology.len() > MAX_PROGRESS_NODES {
            return Err(invalid_data(format!(
                "workflow progress exceeds the {MAX_PROGRESS_NODES}-node cap"
            )));
        }
        validate_text("workflow name", &self.name)?;
        validate_text("workflow args digest", &self.args_digest)?;
        if self.state == DurableRunState::Terminal
            && matches!(
                self.status,
                DurableRunStatus::PendingInit | DurableRunStatus::Running
            )
        {
            return Err(invalid_data(
                "terminal workflow progress has a non-terminal status",
            ));
        }
        validate_run_status(&self.status)?;
        for (index, phase) in &self.phases {
            if index != &phase.index {
                return Err(invalid_data("workflow progress phase key mismatch"));
            }
            if *index >= MAX_PROGRESS_PHASES as u64 {
                return Err(invalid_data(
                    "workflow progress phase index exceeds its cap",
                ));
            }
            validate_text("workflow phase title", &phase.title)?;
        }
        for (node_id, node) in &self.topology {
            if node_id != &node.id() {
                return Err(invalid_data("workflow progress topology key mismatch"));
            }
            if let DurableNode::Agent(agent) = node {
                if let Some(label) = &agent.label {
                    validate_text("workflow agent label", label)?;
                }
                if let Some(model) = &agent.model {
                    validate_text("workflow agent model", model)?;
                }
                if let Some(thread_id) = &agent.child_thread_id {
                    uuid::Uuid::parse_str(thread_id).map_err(|error| {
                        invalid_data(format!(
                            "workflow agent {node_id} has invalid child thread id: {error}"
                        ))
                    })?;
                }
                validate_agent_status(&agent.status)?;
                validate_token_usage(&agent.token_usage)?;
            }
        }
        if let Some(budget) = self.budget
            && (budget.spent < 0 || budget.total.is_some_and(|total| total < 0))
        {
            return Err(invalid_data("workflow progress has a negative budget"));
        }
        Ok(())
    }
}

fn validate_agent_status(status: &AgentStatus) -> io::Result<()> {
    match status {
        AgentStatus::Completed(Some(message)) | AgentStatus::Errored(message) => {
            validate_text("workflow agent status message", message)
        }
        AgentStatus::PendingInit
        | AgentStatus::Running
        | AgentStatus::Interrupted
        | AgentStatus::Completed(None)
        | AgentStatus::Shutdown
        | AgentStatus::NotFound => Ok(()),
    }
}

fn validate_run_status(status: &DurableRunStatus) -> io::Result<()> {
    match status {
        DurableRunStatus::Completed(Some(message)) | DurableRunStatus::Errored(message) => {
            validate_text("workflow run status message", message)
        }
        DurableRunStatus::PendingInit
        | DurableRunStatus::Running
        | DurableRunStatus::Interrupted
        | DurableRunStatus::Completed(None)
        | DurableRunStatus::Shutdown
        | DurableRunStatus::NotFound
        | DurableRunStatus::Stopped
        | DurableRunStatus::Paused => Ok(()),
    }
}

fn validate_token_usage(usage: &TokenUsage) -> io::Result<()> {
    if [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.total_tokens,
    ]
    .into_iter()
    .any(|value| value < 0)
    {
        Err(invalid_data("workflow progress has negative token usage"))
    } else {
        Ok(())
    }
}

fn validate_text(field: &str, value: &str) -> io::Result<()> {
    if value.len() > MAX_PROGRESS_TEXT_BYTES {
        Err(invalid_data(format!(
            "{field} exceeds the {MAX_PROGRESS_TEXT_BYTES}-byte cap"
        )))
    } else {
        Ok(())
    }
}
