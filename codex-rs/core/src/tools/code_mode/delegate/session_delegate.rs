use super::*;

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                }))
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                }))
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn spawn_agent<'a>(
        &'a self,
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            // A cancelled call or an unavailable/stopped dispatcher resolves the isolate promise to
            // JS `null` (death-is-null) rather than throwing — only an admission-time cap rejection
            // (surfaced by the host as `AgentSpawnOutcome::Rejected`) throws.
            if cancellation_token.is_cancelled() {
                return AgentSpawnOutcome::Failed;
            }
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::SpawnAgent {
                    invocation: WorkflowAgentInvocation {
                        cell_id,
                        prompt,
                        ordinal,
                        opts,
                    },
                    progress_context: WorkflowAgentProgressContext {
                        node_id,
                        parent_node_id,
                        phase,
                    },
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                }))
                .is_err()
            {
                return AgentSpawnOutcome::Failed;
            }
            // The tracked driver owns cancellation through registered-child reap, journal flush,
            // and final progress. Await its answer so parent/session shutdown cannot outrun those
            // terminal side effects.
            response_rx.await.unwrap_or(AgentSpawnOutcome::Failed)
        })
    }

    fn spawn_workflow<'a>(
        &'a self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            // A cancelled call or an unavailable/stopped dispatcher resolves the isolate promise to
            // JS `null` (death-is-null) rather than throwing; only an unresolved name / nested error
            // (surfaced by the host as `Rejected`) throws.
            if cancellation_token.is_cancelled() {
                return AgentSpawnOutcome::Failed;
            }
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::SpawnWorkflow {
                    cell_id,
                    name,
                    args,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                }))
                .is_err()
            {
                return AgentSpawnOutcome::Failed;
            }
            // Cancellation is consumed by the tracked nested driver, which does not answer until
            // terminal journal/lease/route cleanup is complete. Returning directly on the token
            // would detach that cleanup from parent and session shutdown.
            response_rx.await.unwrap_or(AgentSpawnOutcome::Failed)
        })
    }

    fn workflow_budget_snapshot<'a>(&'a self, cell_id: CellId) -> WorkflowBudgetSnapshotFuture<'a> {
        Box::pin(async move {
            Ok(self
                .workflow_run_ledger
                .budget_for_cell(&cell_id)
                .map(|budget| protocol_budget_snapshot(&budget)))
        })
    }

    fn journal_phase<'a>(&'a self, cell_id: CellId, title: String) -> NotificationFuture<'a> {
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::JournalPhase {
                    cell_id,
                    title,
                    response_tx,
                }))
                .map_err(|_| "code mode workflow phase dispatcher is unavailable".to_string())?;
            response_rx
                .await
                .map_err(|_| "code mode workflow phase dispatcher stopped".to_string())?
        })
    }

    fn journal_log<'a>(&'a self, cell_id: CellId, message: String) -> NotificationFuture<'a> {
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::JournalLog {
                    cell_id,
                    message,
                    response_tx,
                }))
                .map_err(|_| "code mode workflow log dispatcher is unavailable".to_string())?;
            response_rx
                .await
                .map_err(|_| "code mode workflow log dispatcher stopped".to_string())?
        })
    }

    fn replay_agent<'a>(
        &'a self,
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        entry: JsonValue,
    ) -> NotificationFuture<'a> {
        // Prefix-replay cache hit (spec §7 "Resume algorithm" step 3): the isolate matched this
        // ordinal's recomputed `(prompt, opts)` key against the journaled entry and served the
        // promise from its `return` WITHOUT spawning. Charge the replayed tokens only to this
        // run's workflow meter so later live calls see the same local headroom, then re-append the
        // entry to the NEW run's journal. Session accounting is intentionally not replayed because
        // no model turn occurred.
        //
        // The entry crosses the wire-shaped protocol seam as JSON (see the trait doc); parse it back
        // into a typed line here. A malformed record cannot be dropped: that would let the runtime
        // continue from an incomplete replay prefix and later mark that run complete.
        let line = serde_json::from_value::<AgentCallLine>(entry).map_err(|error| {
            warn!(%error, "workflow replay record failed to deserialize");
            "code mode workflow replay record is invalid".to_string()
        });
        Box::pin(async move {
            let line = line?;
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::ReplayAgent {
                    cell_id,
                    node_id,
                    parent_node_id,
                    phase,
                    line,
                    cancellation_token: CancellationToken::new(),
                    response_tx,
                }))
                .map_err(|_| "code mode workflow replay dispatcher is unavailable".to_string())?;
            response_rx
                .await
                .map_err(|_| "code mode workflow replay dispatcher stopped".to_string())?
        })
    }

    fn workflow_progress<'a>(
        &'a self,
        cell_id: CellId,
        progress: WorkflowHostProgress,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            let (response_tx, response_rx) = oneshot::channel();
            self.command_tx
                .send(BrokerCommand::dispatch(DispatchMessage::WorkflowProgress {
                    cell_id,
                    progress,
                    response_tx,
                }))
                .map_err(|_| "code mode workflow progress dispatcher is unavailable".to_string())?;
            response_rx
                .await
                .map_err(|_| "code mode workflow progress dispatcher stopped".to_string())?
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        // Workflow lifecycle cleanup owns this route once the cell is registered in the
        // ledger. Keeping it bound here lets cancellation join every nested dispatch before
        // the lifecycle emits its terminal event, flushes the journal, and forgets the mapping.
        // Plain code-mode cells have no such lifecycle and can close immediately.
        if self
            .workflow_run_ledger
            .parent_run_id_for_cell(cell_id)
            .is_none()
        {
            self.close_cell(cell_id);
        }
    }
}
