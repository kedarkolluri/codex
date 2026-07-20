use super::*;

pub(super) enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    SpawnAgent {
        invocation: WorkflowAgentInvocation,
        progress_context: WorkflowAgentProgressContext,
        cancellation_token: CancellationToken,
        // Three-way [`AgentSpawnOutcome`] (SEAM CONTRACT): `Completed(value)` on success (a JSON
        // string when schemaless, or the validated `opts.schema` object), `Failed` on agent
        // death/abort/parse-fail (JS null), and `Rejected(msg)` when a scheduler admission cap or a
        // bounds check refuses the spawn (a JS throw once the seam lands).
        response_tx: oneshot::Sender<AgentSpawnOutcome>,
    },
    ReplayAgent {
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        line: AgentCallLine,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    WorkflowProgress {
        cell_id: CellId,
        progress: WorkflowHostProgress,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    JournalPhase {
        cell_id: CellId,
        title: String,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    JournalLog {
        cell_id: CellId,
        message: String,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    SpawnWorkflow {
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
        // Reuses the [`AgentSpawnOutcome`] seam: `Completed(value)` -> the nested run's top-level
        // result, `Failed` -> JS null, `Rejected(msg)` -> a JS throw (unresolved name / nested error).
        response_tx: oneshot::Sender<AgentSpawnOutcome>,
    },
}

impl DispatchMessage {
    pub(super) fn cell_id(&self) -> &CellId {
        match self {
            Self::InvokeTool { invocation, .. } => &invocation.cell_id,
            Self::SpawnAgent { invocation, .. } => &invocation.cell_id,
            Self::Notify { cell_id, .. }
            | Self::ReplayAgent { cell_id, .. }
            | Self::WorkflowProgress { cell_id, .. }
            | Self::JournalPhase { cell_id, .. }
            | Self::JournalLog { cell_id, .. }
            | Self::SpawnWorkflow { cell_id, .. } => cell_id,
        }
    }

    pub(super) fn fail(self, reason: String) {
        match self {
            Self::InvokeTool { response_tx, .. } => {
                let _ = response_tx.send(Err(reason));
            }
            Self::Notify { response_tx, .. }
            | Self::ReplayAgent { response_tx, .. }
            | Self::WorkflowProgress { response_tx, .. }
            | Self::JournalPhase { response_tx, .. }
            | Self::JournalLog { response_tx, .. } => {
                let _ = response_tx.send(Err(reason));
            }
            Self::SpawnAgent { response_tx, .. } | Self::SpawnWorkflow { response_tx, .. } => {
                warn!("{reason}");
                let _ = response_tx.send(AgentSpawnOutcome::Failed);
            }
        }
    }
}

pub(super) async fn run_cell_dispatcher(
    host: BoundDispatchHost,
    mut command_rx: mpsc::UnboundedReceiver<CellDispatchCommand>,
    workflow_run_ledger: Arc<WorkflowRunLedger>,
    dispatch_tasks: TaskTracker,
    cell_tasks: TaskTracker,
    workflow_dispatch_shutdown: CancellationToken,
    dispatcher_done: CancellationToken,
) {
    let _dispatcher_done = dispatcher_done.drop_guard();
    let mut ready = false;
    let mut pending = VecDeque::new();
    while let Some(command) = command_rx.recv().await {
        match command {
            CellDispatchCommand::Dispatch(message) if ready => {
                dispatch_ready_message(
                    &host,
                    &workflow_run_ledger,
                    &dispatch_tasks,
                    &cell_tasks,
                    &workflow_dispatch_shutdown,
                    message,
                )
                .await;
            }
            CellDispatchCommand::Dispatch(message) => {
                if pending.len() >= MAX_PREPARED_CELL_MESSAGES {
                    let cell_id = message.cell_id().clone();
                    message.fail(format!(
                        "code mode cell `{cell_id}` exceeded the pre-ready dispatch queue limit"
                    ));
                } else {
                    pending.push_back(message);
                }
            }
            CellDispatchCommand::Ready => {
                ready = true;
                while let Some(message) = pending.pop_front() {
                    dispatch_ready_message(
                        &host,
                        &workflow_run_ledger,
                        &dispatch_tasks,
                        &cell_tasks,
                        &workflow_dispatch_shutdown,
                        message,
                    )
                    .await;
                }
            }
            CellDispatchCommand::Close(reason) => {
                for message in pending {
                    message.fail(reason.clone());
                }
                return;
            }
        }
    }
    for message in pending {
        message.fail("code mode cell dispatcher stopped before becoming ready".to_string());
    }
}

async fn dispatch_ready_message(
    host: &BoundDispatchHost,
    workflow_run_ledger: &WorkflowRunLedger,
    dispatch_tasks: &TaskTracker,
    cell_tasks: &TaskTracker,
    workflow_dispatch_shutdown: &CancellationToken,
    message: Box<DispatchMessage>,
) {
    match *message {
        DispatchMessage::InvokeTool {
            invocation,
            cancellation_token,
            response_tx,
        } => {
            let BoundDispatchHost::Core(host) = host else {
                let _ = response_tx.send(Err(
                    "code mode cell has no nested-tool dispatch host".to_string()
                ));
                return;
            };
            let host = Arc::clone(host);
            tokio::spawn(async move {
                let response = tokio::select! {
                    response = host.invoke_tool(invocation, cancellation_token.clone()) => response,
                    _ = cancellation_token.cancelled() => {
                        Err("code mode nested tool call cancelled".to_string())
                    }
                };
                let _ = response_tx.send(response);
            });
        }
        DispatchMessage::Notify {
            call_id,
            cell_id,
            text,
            cancellation_token,
            response_tx,
        } => {
            let response = match host {
                BoundDispatchHost::Core(host) => tokio::select! {
                    response = host.notify(call_id, cell_id, text) => response,
                    _ = cancellation_token.cancelled() => {
                        Err("code mode notification cancelled".to_string())
                    }
                },
                #[cfg(test)]
                BoundDispatchHost::Test(host) => {
                    let record = TestDispatchRecord {
                        owner_id: host.owner_id.clone(),
                        cell_id,
                        text,
                    };
                    match host.records.lock() {
                        Ok(mut records) => records.push(record),
                        Err(poisoned) => poisoned.into_inner().push(record),
                    }
                    Ok(())
                }
                BoundDispatchHost::Disabled => {
                    Err("code mode cell has no notification dispatch host".to_string())
                }
            };
            let _ = response_tx.send(response);
        }
        DispatchMessage::SpawnAgent {
            invocation,
            progress_context,
            cancellation_token,
            response_tx,
        } => {
            let BoundDispatchHost::Core(host) = host else {
                let _ = response_tx.send(AgentSpawnOutcome::Failed);
                return;
            };
            let host = Arc::clone(host);
            let shutdown = workflow_dispatch_shutdown.clone();
            dispatch_tasks.spawn(cell_tasks.track_future(async move {
                let driver_cancellation = CancellationToken::new();
                let run =
                    host.spawn_agent(invocation, progress_context, driver_cancellation.clone());
                tokio::pin!(run);
                let result = tokio::select! {
                    biased;
                    result = &mut run => result,
                    _ = cancellation_token.cancelled() => {
                        driver_cancellation.cancel();
                        run.await
                    }
                    _ = shutdown.cancelled() => {
                        driver_cancellation.cancel();
                        run.await
                    }
                };
                let _ = response_tx.send(result);
            }));
        }
        DispatchMessage::ReplayAgent {
            cell_id,
            node_id,
            parent_node_id,
            phase,
            line,
            cancellation_token,
            response_tx,
        } => {
            let response = match host {
                BoundDispatchHost::Core(host) => tokio::select! {
                    response = host.replay_agent(cell_id, node_id, parent_node_id, phase, line) => {
                        response
                    }
                    _ = cancellation_token.cancelled() => {
                        Err("code mode workflow replay cancelled".to_string())
                    }
                },
                #[cfg(test)]
                BoundDispatchHost::Test(_) => Ok(()),
                BoundDispatchHost::Disabled => Ok(()),
            };
            let _ = response_tx.send(response);
        }
        DispatchMessage::WorkflowProgress {
            cell_id,
            progress,
            response_tx,
        } => {
            match host {
                BoundDispatchHost::Core(host) => host.workflow_progress(cell_id, progress).await,
                #[cfg(test)]
                BoundDispatchHost::Test(_) => {}
                BoundDispatchHost::Disabled => {}
            }
            let _ = response_tx.send(Ok(()));
        }
        DispatchMessage::JournalPhase {
            cell_id,
            title,
            response_tx,
        } => {
            let response = if let Some(recorder) = workflow_run_ledger.recorder_for_cell(&cell_id) {
                recorder
                    .record_phase(PhaseLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        title,
                    })
                    .await
                    .map_err(|err| err.to_string())
            } else {
                Ok(())
            };
            let _ = response_tx.send(response);
        }
        DispatchMessage::JournalLog {
            cell_id,
            message,
            response_tx,
        } => {
            let response = if let Some(recorder) = workflow_run_ledger.recorder_for_cell(&cell_id) {
                recorder
                    .record_log(LogLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        message,
                    })
                    .await
                    .map_err(|err| err.to_string())
            } else {
                Ok(())
            };
            let _ = response_tx.send(response);
        }
        DispatchMessage::SpawnWorkflow {
            cell_id,
            name,
            args,
            cancellation_token,
            response_tx,
        } => {
            let BoundDispatchHost::Core(host) = host else {
                let _ = response_tx.send(AgentSpawnOutcome::Failed);
                return;
            };
            let host = Arc::clone(host);
            let shutdown = workflow_dispatch_shutdown.clone();
            dispatch_tasks.spawn(cell_tasks.track_future(async move {
                // Merge parent-cell and broker-shutdown cancellation into a driver-owned token.
                // After cancellation wins, keep polling the terminal runner until it has
                // terminated the nested cell, flushed the journal, closed dispatch state, and
                // released the run lease; racing and dropping that future would bypass cleanup.
                let driver_cancellation = CancellationToken::new();
                let run = host.spawn_workflow(cell_id, name, args, driver_cancellation.clone());
                tokio::pin!(run);
                let result = tokio::select! {
                    biased;
                    result = &mut run => result,
                    _ = cancellation_token.cancelled() => {
                        driver_cancellation.cancel();
                        run.await
                    }
                    _ = shutdown.cancelled() => {
                        driver_cancellation.cancel();
                        run.await
                    }
                };
                let _ = response_tx.send(result);
            }));
        }
    }
}
