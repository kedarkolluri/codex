use std::time::Instant;

use super::*;

#[derive(Clone)]
pub(super) struct WorkflowAgentInvocation {
    pub(super) cell_id: CellId,
    pub(super) prompt: String,
    pub(super) ordinal: u64,
    pub(super) opts: AgentCallOpts,
}

#[derive(Clone)]
pub(super) struct WorkflowAgentProgressContext {
    pub(super) node_id: u64,
    pub(super) parent_node_id: Option<u64>,
    pub(super) phase: Option<String>,
}

pub(super) struct WorkflowAgentExecutionContext {
    pub(super) config: crate::config::Config,
    pub(super) observer: WorkflowChildObserver,
    pub(super) run_budget: Arc<WorkflowBudget>,
    pub(super) cancellation_token: CancellationToken,
    pub(super) journal: Arc<AgentCallJournalCtx>,
}

pub(super) struct WorkflowAgentAttemptContext {
    pub(super) attempt: u32,
    pub(super) cancellation_token: CancellationToken,
    pub(super) journal: Arc<AgentCallJournalCtx>,
    pub(super) prior_progress: WorkflowChildProgress,
    pub(super) prior_duration_ms: u64,
    pub(super) started_at: Instant,
    pub(super) activation: WorkflowAgentAttemptActivation,
}

pub(super) struct WorkflowAgentAttemptResult {
    pub(super) execution: WorkflowAgentExecution,
    pub(super) progress: WorkflowChildProgress,
    pub(super) bound: bool,
}

pub(super) struct CoreTurnHost {
    pub(super) exec: ExecContext,
    pub(super) tool_runtime: ToolCallRuntime,
    /// Per-run concurrency + lifetime scheduler shared by every `agent()` call in this workflow run
    /// (spec §5). Constructed once in `start_turn_worker`; `admit` bounds concurrent spawns and
    /// enforces the monotonic lifetime cap.
    pub(super) scheduler: WorkflowScheduler,
}

impl CoreTurnHost {
    pub(super) async fn invoke_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        call_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation_token,
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Route a workflow `agent(prompt, opts?)` call through the per-run [`WorkflowScheduler`] into
    /// the wave-2 spawn keystone
    /// [`crate::agent::control::AgentControl::spawn_and_await_final_message`], resolving to an
    /// [`AgentSpawnOutcome`].
    ///
    /// ## Admission (spec §5) & determinism
    ///
    /// Before spawning, the incoming prompt and any `opts.schema` are bounded; either over-limit
    /// value returns [`AgentSpawnOutcome::Rejected`] without consuming a lifetime slot. The call is
    /// then admitted through [`WorkflowScheduler::admit`]: the monotonic lifetime CAS runs first
    /// (over-cap -> `Rejected("AgentCapReached")`, no permit awaited), then a concurrency permit is
    /// held across the spawn and released on finalize on every path. The child nickname is derived
    /// purely from the invocation `ordinal` via [`workflow_agent_nickname_preference`] (no `rand`).
    ///
    /// The keystone constructs the `Subagent` source itself and drives the child's first turn to
    /// completion over the non-competing event tap. A normal final message becomes
    /// [`AgentSpawnOutcome::Completed`]; a dead/aborted child (or a config-build/spawn/submit
    /// failure, or a schema parse/validation failure) becomes [`AgentSpawnOutcome::Failed`] (JS
    /// null). `opts.model` / `opts.effort` / `opts.agentType` are threaded as
    /// [`SpawnAgentConfigOverrides`] and applied to the inherited child config before the spawn;
    /// omitted overrides inherit the parent turn.
    ///
    /// ## Structured output (`opts.schema`, spec §6)
    ///
    /// When `opts.schema` is present it is threaded onto the child's first turn as
    /// `final_output_json_schema`, forcing a StructuredOutput (`output_schema_strict = true`) final
    /// message. On return the raw final text is `serde_json`-parsed and, as **defense-in-depth**
    /// (engine strict mode is enforced for OpenAI providers but not guaranteed for all), re-validated
    /// against the JSON Schema with the `jsonschema` crate before the parsed object is resolved back
    /// to JS. A parse or validation failure resolves to `None` (JS `null`) per the death-is-null
    /// contract. Without `opts.schema` the plain final text is resolved as a JSON string. See
    /// [`finalize_agent_output`].
    pub(super) async fn spawn_agent_attempt(
        &self,
        invocation: WorkflowAgentInvocation,
        progress_context: WorkflowAgentProgressContext,
        context: WorkflowAgentAttemptContext,
    ) -> WorkflowAgentAttemptResult {
        let WorkflowAgentAttemptContext {
            attempt,
            cancellation_token,
            journal,
            prior_progress,
            prior_duration_ms,
            started_at,
            activation,
        } = context;
        let WorkflowAgentProgressContext {
            node_id,
            parent_node_id,
            phase,
        } = progress_context;
        let session = &self.exec.session;
        let turn = self.exec.turn.as_ref();
        let overrides = SpawnAgentConfigOverrides {
            model: invocation.opts.model.clone(),
            effort: invocation.opts.effort.clone(),
            agent_type: invocation.opts.agent_type.clone(),
        };
        let base_instructions = session.get_base_instructions().await;
        let parent_thread_id = session.thread_id;
        let config = session
            .services
            .agent_control
            .prepare_workflow_spawn_config(&base_instructions, turn, parent_thread_id, &overrides)
            .await;
        let model = config
            .as_ref()
            .and_then(|config| config.model.clone())
            .unwrap_or_else(|| turn.model_info.slug.clone());
        let effort = config
            .as_ref()
            .and_then(|config| config.model_reasoning_effort.clone())
            .or_else(|| turn.reasoning_effort.clone())
            .unwrap_or(ReasoningEffort::None);
        let ledger = session.services.code_mode_service.workflow_run_ledger();
        let Some(run_budget) = ledger.budget_for_cell(&invocation.cell_id) else {
            warn!(
                "workflow agent() rejected: no run-local budget for cell {}",
                invocation.cell_id
            );
            return WorkflowAgentAttemptResult {
                execution: WorkflowAgentExecution::finished(AgentSpawnOutcome::Failed),
                progress: WorkflowChildProgress::default(),
                bound: false,
            };
        };
        let Some(config) = config else {
            warn!("workflow agent() rejected: failed to resolve workflow agent config");
            return WorkflowAgentAttemptResult {
                execution: WorkflowAgentExecution::finished(AgentSpawnOutcome::Failed),
                progress: WorkflowChildProgress::default(),
                bound: false,
            };
        };

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut observer = WorkflowChildObserver::new(progress_tx);
        if let Some(recorder) = ledger.recorder_for_cell(&invocation.cell_id) {
            observer = observer.with_binding_journal(recorder, invocation.ordinal, attempt);
        }
        let progress_exec = self.exec.clone();
        let progress_cell_id = invocation.cell_id.clone();
        let progress_label = invocation.opts.label.clone();
        let ordinal = invocation.ordinal;
        let last_attempt_reason = (attempt > 0).then_some(WorkflowAgentAttemptReason::UserRetry);
        let progress_task = tokio::spawn(async move {
            let mut latest = WorkflowChildProgress::default();
            let mut bound = false;
            while let Some(event) = progress_rx.recv().await {
                let ledger = progress_exec
                    .session
                    .services
                    .code_mode_service
                    .workflow_run_ledger();
                match event {
                    WorkflowChildEvent::Bound {
                        child_thread_id,
                        acknowledged,
                    } if !bound => {
                        let began = workflow_progress::emit_agent_begin(
                            &progress_exec,
                            ledger,
                            &progress_cell_id,
                            workflow_progress::WorkflowAgentBeginParams {
                                node_id,
                                attempt,
                                last_attempt_reason,
                                parent_node_id,
                                requested_label: progress_label.as_deref(),
                                ordinal,
                                phase: phase.clone(),
                                model: model.clone(),
                                effort: effort.clone(),
                            },
                        )
                        .await;
                        let binding_result = if !began {
                            Err(format!(
                                "failed to emit workflow agent begin for node {node_id}"
                            ))
                        } else {
                            // The child binding is already durable at this point. Publish the
                            // attempt as selectable before sending AgentBound so a consumer that
                            // reacts immediately to that event cannot lose an activation race.
                            activation.activate();
                            if workflow_progress::emit_agent_bound(
                                &progress_exec,
                                ledger,
                                &progress_cell_id,
                                node_id,
                                attempt,
                                child_thread_id,
                            )
                            .await
                            {
                                bound = true;
                                Ok(())
                            } else {
                                Err(format!(
                                    "failed to emit workflow agent binding for node {node_id}"
                                ))
                            }
                        };
                        let _ = acknowledged.send(binding_result);
                    }
                    WorkflowChildEvent::Bound { acknowledged, .. } => {
                        let _ = acknowledged.send(Err(format!(
                            "workflow agent node {node_id} was bound more than once"
                        )));
                    }
                    WorkflowChildEvent::Progress(progress) if bound => {
                        latest = progress;
                        let aggregate = add_workflow_child_progress(&prior_progress, &latest);
                        let duration_ms = prior_duration_ms.saturating_add(
                            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        workflow_progress::emit_agent_update(
                            &progress_exec,
                            ledger,
                            &progress_cell_id,
                            workflow_progress::WorkflowAgentUpdateParams {
                                node_id,
                                attempt,
                                last_attempt_reason,
                                token_usage: aggregate.token_usage,
                                tool_call_count: aggregate.tool_call_count,
                                duration_ms,
                            },
                        )
                        .await;
                    }
                    WorkflowChildEvent::Progress(_) => {}
                }
            }
            (latest, bound)
        });
        let execution = self
            .spawn_agent_inner(
                invocation,
                attempt,
                WorkflowAgentExecutionContext {
                    config,
                    observer,
                    run_budget,
                    cancellation_token,
                    journal,
                },
            )
            .await;
        let (progress, bound) = progress_task.await.unwrap_or_default();
        WorkflowAgentAttemptResult {
            execution,
            progress,
            bound,
        }
    }
}

pub(super) fn add_workflow_child_progress(
    prior: &WorkflowChildProgress,
    current: &WorkflowChildProgress,
) -> WorkflowChildProgress {
    WorkflowChildProgress {
        token_usage: add_token_usage(&prior.token_usage, &current.token_usage),
        tool_call_count: prior
            .tool_call_count
            .saturating_add(current.tool_call_count),
    }
}

fn add_token_usage(left: &TokenUsage, right: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        reasoning_output_tokens: left
            .reasoning_output_tokens
            .saturating_add(right.reasoning_output_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
    }
}
