use super::*;

impl CoreTurnHost {
    /// Route a workflow `workflow(nameOrRef, args)` call to the registry-load + nested re-enter
    /// handler ([`run_workflow_by_name`]), resolving to an [`AgentSpawnOutcome`] the isolate settles
    /// the `workflow()` promise with. `cell_id` is the PARENT cell that made the call, used to
    /// recover the parent run id for the nested run's `parent_run_id`.
    pub(super) async fn spawn_workflow(
        &self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnOutcome {
        run_workflow_by_name(&self.exec, &cell_id, &name, args, cancellation_token).await
    }

    pub(super) async fn replay_agent(
        &self,
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        line: AgentCallLine,
    ) -> Result<(), String> {
        let child_thread_id = line
            .child_thread_id
            .as_deref()
            .ok_or_else(|| {
                format!(
                    "workflow replay agent ordinal {} has no child thread binding",
                    line.ordinal
                )
            })
            .and_then(|thread_id| {
                ThreadId::from_string(thread_id).map_err(|error| {
                    format!(
                        "workflow replay agent ordinal {} has invalid child thread binding: {error}",
                        line.ordinal
                    )
                })
            })?;
        let ledger = self
            .exec
            .session
            .services
            .code_mode_service
            .workflow_run_ledger();
        let budget = ledger
            .budget_for_cell(&cell_id)
            .ok_or_else(|| format!("workflow replay has no run-local budget for cell {cell_id}"))?;
        if let Some(recorder) = ledger.recorder_for_cell(&cell_id) {
            recorder
                .record_agent_call(line.clone())
                .await
                .map_err(|err| err.to_string())?;
        }
        if let Some(tokens) = line.tokens_spent {
            budget.record_replayed_spent(tokens);
        }

        let turn = self.exec.turn.as_ref();
        let base_instructions = self.exec.session.get_base_instructions().await;
        let overrides = SpawnAgentConfigOverrides {
            model: line.opts.model.clone(),
            effort: line.opts.effort.clone(),
            agent_type: line.opts.agent_type.clone(),
        };
        let config = self
            .exec
            .session
            .services
            .agent_control
            .prepare_workflow_spawn_config(
                &base_instructions,
                turn,
                self.exec.session.thread_id,
                &overrides,
            )
            .await;
        let model = config
            .as_ref()
            .and_then(|config| config.model.clone())
            .unwrap_or_else(|| turn.model_info.slug.clone());
        let effort = config
            .and_then(|config| config.model_reasoning_effort)
            .or_else(|| turn.reasoning_effort.clone())
            .unwrap_or(ReasoningEffort::None);
        // A final replay anchor records only the generation that settled the logical call. Rebuild
        // every preceding Begin transition so the same strict live projector can reject arbitrary
        // generation jumps while accepting this bounded durable replay.
        for attempt in 0..=line.attempt {
            let began = workflow_progress::emit_agent_begin(
                &self.exec,
                ledger,
                &cell_id,
                workflow_progress::WorkflowAgentBeginParams {
                    node_id,
                    attempt,
                    last_attempt_reason: (attempt > 0)
                        .then_some(WorkflowAgentAttemptReason::UserRetry),
                    parent_node_id,
                    requested_label: line.label.as_deref(),
                    ordinal: line.ordinal,
                    phase: phase.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                },
            )
            .await;
            if !began {
                return Err(format!(
                    "workflow replay could not publish attempt {attempt} for node {node_id}"
                ));
            }
        }
        if !workflow_progress::emit_agent_bound(
            &self.exec,
            ledger,
            &cell_id,
            node_id,
            line.attempt,
            child_thread_id,
        )
        .await
        {
            return Err(format!(
                "workflow replay could not publish child binding for node {node_id}"
            ));
        }
        let (mut progress, duration_ms) = if let Some(recorded) = &line.progress {
            (
                WorkflowChildProgress {
                    token_usage: TokenUsage {
                        input_tokens: recorded.token_usage.input_tokens,
                        cached_input_tokens: recorded.token_usage.cached_input_tokens,
                        output_tokens: recorded.token_usage.output_tokens,
                        reasoning_output_tokens: recorded.token_usage.reasoning_output_tokens,
                        total_tokens: recorded.token_usage.total_tokens,
                    },
                    tool_call_count: recorded.tool_call_count,
                },
                recorded.duration_ms,
            )
        } else {
            (
                crate::agent::control::workflow_child_progress::replay_progress(
                    line.rollout_path.as_deref(),
                )
                .await,
                0,
            )
        };
        if progress.token_usage == Default::default()
            && let Some(tokens) = line.tokens_spent
        {
            let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
            progress.token_usage.output_tokens = tokens;
            progress.token_usage.total_tokens = tokens;
        }
        workflow_progress::emit_agent_update(
            &self.exec,
            ledger,
            &cell_id,
            workflow_progress::WorkflowAgentUpdateParams {
                node_id,
                attempt: line.attempt,
                last_attempt_reason: (line.attempt > 0)
                    .then_some(WorkflowAgentAttemptReason::UserRetry),
                token_usage: progress.token_usage.clone(),
                tool_call_count: progress.tool_call_count,
                duration_ms,
            },
        )
        .await;
        let (last_attempt_reason, status) = match line.control_reason {
            Some(JournalAgentControlReason::UserSkip) => (
                Some(WorkflowAgentAttemptReason::UserSkip),
                AgentStatus::Shutdown,
            ),
            Some(JournalAgentControlReason::RetryLimitReached) => (
                Some(WorkflowAgentAttemptReason::RetryLimitReached),
                AgentStatus::Errored(WORKFLOW_AGENT_RETRY_LIMIT_ERROR.to_string()),
            ),
            None => (
                (line.attempt > 0).then_some(WorkflowAgentAttemptReason::UserRetry),
                AgentStatus::Completed(None),
            ),
        };
        workflow_progress::emit_agent_end(
            &self.exec,
            ledger,
            &cell_id,
            node_id,
            line.attempt,
            last_attempt_reason,
            status,
            progress.token_usage,
            progress.tool_call_count,
            duration_ms,
            line.ret.is_null(),
        )
        .await;

        Ok(())
    }

    pub(super) async fn workflow_progress(&self, cell_id: CellId, progress: WorkflowHostProgress) {
        let ledger = self
            .exec
            .session
            .services
            .code_mode_service
            .workflow_run_ledger();
        workflow_progress::handle_host_progress(&self.exec, ledger, &cell_id, progress).await;
    }

    pub(super) async fn notify(
        &self,
        call_id: String,
        cell_id: CellId,
        text: String,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_if_running(vec![ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .map_err(|_| {
                format!("failed to inject exec notify message for cell {cell_id}: no active turn")
            })
    }
}
