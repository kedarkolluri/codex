use super::*;

pub(super) fn reject_worktree_setup(
    error: &(impl std::fmt::Display + ?Sized),
) -> WorkflowAgentExecution {
    warn!(error = %error, "workflow agent worktree setup failed");
    WorkflowAgentExecution::finished(AgentSpawnOutcome::Rejected(
        WORKFLOW_WORKTREE_SETUP_FAILED.to_string(),
    ))
}

pub(super) fn validate_agent_invocation(
    invocation: &WorkflowAgentInvocation,
) -> Result<(), String> {
    ensure_prompt_within_bounds(&invocation.prompt)?;
    ensure_agent_options_within_bounds(
        invocation.opts.label.as_deref(),
        invocation.opts.phase.as_deref(),
        invocation.opts.model.as_deref(),
        invocation.opts.effort.as_deref(),
        invocation.opts.agent_type.as_deref(),
        invocation.opts.isolation.as_deref(),
    )?;
    if let Some(schema) = invocation.opts.schema.as_ref() {
        ensure_schema_within_bounds(schema)?;
    }
    Ok(())
}

impl CoreTurnHost {
    pub(super) async fn spawn_agent_inner(
        &self,
        invocation: WorkflowAgentInvocation,
        attempt: u32,
        context: WorkflowAgentExecutionContext,
    ) -> WorkflowAgentExecution {
        let WorkflowAgentInvocation {
            cell_id,
            prompt,
            ordinal,
            opts,
        } = invocation;
        let WorkflowAgentExecutionContext {
            config,
            observer,
            run_budget,
            cancellation_token,
            journal,
        } = context;

        let isolation = match WorkflowAgentIsolation::parse(opts.isolation.as_deref()) {
            Ok(isolation) => isolation,
            Err(err) => {
                let reason = err.to_string();
                warn!("workflow agent() rejected: {reason}");
                return WorkflowAgentExecution::finished(AgentSpawnOutcome::Rejected(reason));
            }
        };
        let ledger = self
            .exec
            .session
            .services
            .code_mode_service
            .workflow_run_ledger();
        let worktree_plan = match isolation {
            WorkflowAgentIsolation::Inherited => None,
            WorkflowAgentIsolation::Worktree => {
                let execution_environment =
                    match WorktreeExecutionEnvironment::from_snapshot(&self.exec.turn.environments)
                    {
                        Ok(execution_environment) => execution_environment,
                        Err(err) => {
                            return reject_worktree_setup(&err);
                        }
                    };
                let Some(run_id) = ledger.parent_run_id_for_cell(&cell_id) else {
                    let reason = WorktreeIsolationError::MissingRunIdentity.to_string();
                    warn!("workflow agent() rejected: {reason}");
                    return WorkflowAgentExecution::finished(AgentSpawnOutcome::Rejected(reason));
                };
                match execution_environment.allocation(&run_id, ordinal, attempt) {
                    Ok(allocation) => Some((allocation, execution_environment)),
                    Err(err) => return reject_worktree_setup(&err),
                }
            }
        };

        let session = &self.exec.session;

        // Budget governance uses two independent meters: RolloutBudget remains the immutable
        // session-tree ceiling, while WorkflowBudget accounts only for this run and its nested
        // descendants. A workflow never reconfigures session policy.
        //
        // Enforcement has two parts:
        //  1. A cheap, best-effort pre-check fast-rejects either exhausted meter without consuming
        //     a lifetime slot. The read is racy under concurrency, so it is not the ceiling enforcer.
        //  2. Inside the concurrency-permit region, owned RAII reservations are taken from both
        //     meters. Concurrent admissions serialize independently at each ceiling, and dropping
        //     the callback future releases both reservations on cancellation.
        //
        // A budget rejection is the one case `agent()` THROWS (surfaced as `Rejected` → a JS throw);
        // the death-is-null contract still governs agent death/abort.
        let session_budget = session.services.agent_control.rollout_budget_arc();
        let turn_sub_id = self.exec.turn.sub_id.clone();
        // Reporting half of §8 governance: surface the current budget state on the EXISTING
        // ThreadGoal channel (no new protocol types). On an unmetered run this emits nothing.
        emit_budget_thread_goal(session, &turn_sub_id, &run_budget).await;
        // Single source of truth for the pre-admission ceiling predicate (spec §5 step 1): the same
        // `RolloutBudget::pre_admission_rejects` the unit tests assert against, so the host gate and
        // its coverage can never drift.
        if session_budget.pre_admission_rejects() || run_budget.is_exhausted() {
            warn!("workflow agent() rejected: BudgetExceeded (budget ceiling reached)");
            return WorkflowAgentExecution::finished(AgentSpawnOutcome::Rejected(
                "BudgetExceeded".to_string(),
            ));
        }

        let turn = self.exec.turn.as_ref();
        let scheduler = &self.scheduler;
        let schema = opts.schema;
        let isolated_permissions = turn.config.permissions.clone();
        // Nickname is a pure function of the invocation ordinal (spec §7, no `rand`): the registry's
        // preferred-name branch reserves it verbatim (deterministically resolving any collision).
        let preferred_agent_nickname = Some(workflow_agent_nickname_preference(ordinal as usize));
        let requested_agent_role = opts
            .agent_type
            .as_deref()
            .map(str::trim)
            .filter(|role| !role.is_empty())
            .map(str::to_string);

        // Admit through the shared per-run scheduler (spec §5 admission order): the lifetime CAS
        // (step 2) runs first and rejects over-cap calls with `AgentCapReached` WITHOUT awaiting a
        // permit; then a concurrency permit is acquired (step 4) before the child is spawned, and
        // dropped on finalize on every path (step 6) via the permit RAII guard inside `admit`.
        let admit_result = scheduler
            .admit_cancellable(&cancellation_token, || {
                // Cloned per admission attempt (the scheduler may re-invoke on a registry-backstop
                // requeue); `session`/`turn`/`base_instructions` are cheap shared references.
                let prompt = prompt.clone();
                let schema = schema.clone();
                let config = config.clone();
                let observer = observer.clone();
                let preferred_agent_nickname = preferred_agent_nickname.clone();
                let requested_agent_role = requested_agent_role.clone();
                let turn_sub_id = turn_sub_id.clone();
                let journal = Arc::clone(&journal);
                let session_budget = Arc::clone(&session_budget);
                let run_budget = Arc::clone(&run_budget);
                let worktree_plan = worktree_plan.clone();
                let isolated_permissions = isolated_permissions.clone();
                let cancellation_token = cancellation_token.clone();
                async move {
                    if cancellation_token.is_cancelled() {
                        return SpawnAttempt::Finalized(WorkflowAgentExecution::cancelled());
                    }
                    // Authoritative, race-free gates. Session is reserved first, followed by the
                    // run meter, giving every admission one lock order. Both guards live only for
                    // this concurrency-permit region and release automatically on every return or
                    // cancelled future.
                    let _session_reservation = match session_budget
                        .reserve_owned(WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE)
                    {
                        Ok(reservation) => reservation,
                        Err(_) => {
                            emit_budget_thread_goal(session, &turn_sub_id, &run_budget).await;
                            warn!(
                                "workflow agent() rejected: BudgetExceeded (session ceiling reached)"
                            );
                            return SpawnAttempt::Finalized(WorkflowAgentExecution::finished(
                                AgentSpawnOutcome::Rejected("BudgetExceeded".to_string()),
                            ));
                        }
                    };
                    let _run_reservation = match run_budget
                        .reserve(WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE as u64)
                    {
                        Ok(reservation) => reservation,
                        Err(_) => {
                            // Report the crossing before resolving the rejected promise.
                            emit_budget_thread_goal(session, &turn_sub_id, &run_budget).await;
                            warn!(
                                "workflow agent() rejected: BudgetExceeded (budget ceiling reached)"
                            );
                            return SpawnAttempt::Finalized(WorkflowAgentExecution::finished(
                                AgentSpawnOutcome::Rejected("BudgetExceeded".to_string()),
                            ));
                        }
                    };
                    let (mut worktree_guard, environments) = match worktree_plan {
                        Some((allocation, execution_environment)) => match allocation.create() {
                            Ok(guard) => {
                                let environments =
                                    vec![execution_environment.selection_at(guard.path())];
                                (Some(guard), environments)
                            }
                            Err(err) => {
                                return SpawnAttempt::Finalized(reject_worktree_setup(&err));
                            }
                        },
                        None => (None, turn.environments.to_selections()),
                    };
                    let parent_thread_id = session.thread_id;
                    let spawn_workspace = match worktree_guard.as_ref() {
                        Some(guard) => match SpawnAgentWorkspace::isolated_worktree(
                            guard.path().clone(),
                            guard.git_dir().clone(),
                            isolated_permissions.clone(),
                        ) {
                            Ok(spawn_workspace) => Some(spawn_workspace),
                            Err(err) => {
                                return SpawnAttempt::Finalized(reject_worktree_setup(&err));
                            }
                        },
                        None => None,
                    };
                    let options = SpawnAgentOptions {
                        parent_thread_id: Some(parent_thread_id),
                        environments: Some(environments),
                        spawn_workspace,
                        preferred_agent_nickname,
                        agent_role: requested_agent_role,
                        ..Default::default()
                    };
                    let spawn_outcome = session
                        .services
                        .agent_control
                        .spawn_and_await_journaled_with_config_cancellable(
                            config,
                            turn,
                            parent_thread_id,
                            vec![UserInput::Text {
                                text: prompt,
                                text_elements: Vec::new(),
                            }],
                            schema.clone(),
                            options,
                            Some(observer.clone()),
                            cancellation_token,
                        )
                        .await;
                    if let Some(guard) = worktree_guard.as_mut() {
                        let worktree_path = guard.path().clone();
                        // These diagnostics can contain host-absolute paths and changed filenames.
                        // Keep raw details in local tracing. Journal logs are also CLI narration,
                        // so persist only a path- and filename-free recovery notice there.
                        match guard.close() {
                            Ok(WorktreeCleanupOutcome::Removed) => {}
                            Ok(WorktreeCleanupOutcome::RetainedDirty { diagnostic }) => {
                                warn!(
                                    worktree_path = %worktree_path.display(),
                                    %diagnostic,
                                    "retained dirty workflow agent worktree"
                                );
                                journal
                                    .record_diagnostic(
                                        "workflow agent worktree was retained because it contains uncommitted changes"
                                            .to_string(),
                                    )
                                    .await;
                            }
                            Ok(WorktreeCleanupOutcome::AlreadyClosed) => {}
                            Err(error) => {
                                warn!(
                                    worktree_path = %worktree_path.display(),
                                    %error,
                                    "failed to clean workflow agent worktree"
                                );
                                journal
                                    .record_diagnostic(
                                        "workflow agent worktree cleanup failed".to_string(),
                                    )
                                    .await;
                            }
                        }
                    }
                    observer.progress(WorkflowChildProgress {
                        token_usage: spawn_outcome.token_usage.clone(),
                        tool_call_count: spawn_outcome.tool_call_count,
                    });
                    // A normal final message -> `Completed`; agent death/abort/schema parse-fail ->
                    // `Failed`. Both are terminal `Finalized` outcomes, so the permit releases either
                    // way (the registry-backstop `AgentLimitReached` requeue is a scheduler unit
                    // concern; the keystone maps a saturated-registry spawn error to `None` here).
                    let cancelled = spawn_outcome.cancelled;
                    let outcome =
                        match finalize_agent_output(spawn_outcome.final_text, schema.as_ref()) {
                            Some(value) => AgentSpawnOutcome::Completed(value),
                            None => AgentSpawnOutcome::Failed,
                        };
                    run_budget.record_spent(spawn_outcome.tokens_spent.unwrap_or(0));
                    let child_thread_id = spawn_outcome.child_thread_id.map(|id| id.to_string());
                    let rollout_path = spawn_outcome
                        .rollout_path
                        .as_ref()
                        .map(|path| path.display().to_string());
                    // Emit the post-finalize budget state so the crossing turn — even the final
                    // over-ceiling child that ends the run — surfaces `BudgetLimited` (finding #9),
                    // not only when the NEXT `agent()` is attempted.
                    emit_budget_thread_goal(session, &turn_sub_id, &run_budget).await;
                    SpawnAttempt::Finalized(WorkflowAgentExecution {
                        outcome,
                        cancelled,
                        child_thread_id,
                        rollout_path,
                        tokens_spent: spawn_outcome.tokens_spent,
                    })
                }
            })
            .await;

        match admit_result {
            Ok(WorkflowAdmission::Finalized(execution)) => execution,
            Ok(WorkflowAdmission::Cancelled) => WorkflowAgentExecution::cancelled(),
            // Lifetime cap reached (spec §5): terminal and monotonic — surfaced as a JS throw.
            Err(AgentCapReached { .. }) => WorkflowAgentExecution::finished(
                AgentSpawnOutcome::Rejected("AgentCapReached".to_string()),
            ),
        }
    }
}
