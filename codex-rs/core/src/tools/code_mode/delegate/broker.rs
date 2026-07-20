use super::*;

pub(super) const MAX_UNBOUND_DISPATCH_MESSAGES: usize = 4_096;
pub(super) const MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL: usize = 256;
pub(super) const MAX_PREPARED_CELL_MESSAGES: usize = 4_096;
pub(super) const CLOSED_CELL_TOMBSTONE_CAP: usize = 4_096;

/// The explicit owner captured before a code-mode cell is created. Top-level cells bind to the
/// sampling turn registration that advertised the tool; nested workflow cells bind through their
/// parent cell so they keep the exact original context after that turn's registration guard drops.
#[derive(Clone, Debug)]
pub(crate) enum CodeModeDispatchOrigin {
    Turn(String),
    ParentCell(CellId),
    #[cfg(test)]
    Disabled,
}

pub(crate) struct CodeModeDispatchBroker {
    pub(super) command_tx: mpsc::UnboundedSender<BrokerCommand>,
    pub(super) dispatcher_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) dispatcher_shutdown: CancellationToken,
    pub(super) dispatch_tasks: TaskTracker,
    pub(super) workflow_dispatch_shutdown: CancellationToken,
    /// Shared run→parent ledger, forgotten per closed cell (see [`WorkflowRunLedger`]).
    pub(super) workflow_run_ledger: Arc<WorkflowRunLedger>,
}

impl CodeModeDispatchBroker {
    pub(crate) fn new(workflow_run_ledger: Arc<WorkflowRunLedger>) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let dispatcher_shutdown = CancellationToken::new();
        let dispatch_tasks = TaskTracker::new();
        let workflow_dispatch_shutdown = CancellationToken::new();
        let dispatcher_task = tokio::spawn(run_dispatch_broker(
            command_rx,
            Arc::clone(&workflow_run_ledger),
            dispatcher_shutdown.clone(),
            dispatch_tasks.clone(),
            workflow_dispatch_shutdown.clone(),
        ));
        Self {
            command_tx,
            dispatcher_task: Mutex::new(Some(dispatcher_task)),
            dispatcher_shutdown,
            dispatch_tasks,
            workflow_dispatch_shutdown,
            workflow_run_ledger,
        }
    }

    pub(crate) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        let _ = self.command_tx.send(BrokerCommand::MarkCellReady {
            cell_id: cell_id.clone(),
        });
    }

    pub(crate) fn close_cell(&self, cell_id: &CellId) {
        let _ = self.command_tx.send(BrokerCommand::CloseCell {
            cell_id: cell_id.clone(),
        });
        // Drop the cell→run_id entry once its cell is gone so the ledger map does
        // not grow across a long session; the append-only run→parent links stay.
        self.workflow_run_ledger.forget_cell(cell_id);
    }

    /// Close a workflow cell's route and join its registered agent/nested-workflow callbacks while
    /// retaining the ledger/recorder mapping for the caller's terminal event and journal flush.
    pub(crate) async fn drain_workflow_cell(&self, cell_id: &CellId) {
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(BrokerCommand::CloseCellAndWait {
                cell_id: cell_id.clone(),
                response_tx,
            })
            .is_ok()
        {
            let _ = response_rx.await;
        }
    }

    pub(crate) async fn begin_cell_execution(
        &self,
        origin: CodeModeDispatchOrigin,
    ) -> Result<PendingCellExecution, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(BrokerCommand::BeginCellExecution {
                origin,
                response_tx,
            })
            .map_err(|_| "code mode dispatch broker is unavailable".to_string())?;
        let attempt_id = response_rx
            .await
            .map_err(|_| "code mode dispatch broker stopped".to_string())??;
        Ok(PendingCellExecution {
            attempt_id: Some(attempt_id),
            command_tx: self.command_tx.clone(),
        })
    }

    pub(crate) async fn start_turn_worker(
        &self,
        exec: ExecContext,
        router: Arc<ToolRouter>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Result<CodeModeDispatchWorker, String> {
        let owner_id = exec.turn.sub_id.clone();
        let scheduler_max_threads = exec
            .turn
            .config
            .effective_agent_max_threads(exec.turn.multi_agent_version);
        let context = DispatchContext::Core(Arc::new(CoreTurnHostFactory {
            tool_runtime: ToolCallRuntime::new(
                router,
                Arc::clone(&exec.session),
                step_context,
                tracker,
            ),
            exec,
            scheduler_max_threads,
        }));
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(BrokerCommand::RegisterTurn {
                owner_id: owner_id.clone(),
                context,
                response_tx,
            })
            .map_err(|_| "code mode dispatch broker is unavailable".to_string())?;
        let generation = response_rx
            .await
            .map_err(|_| "code mode dispatch broker stopped".to_string())??;
        Ok(CodeModeDispatchWorker {
            command_tx: self.command_tx.clone(),
            owner_id,
            generation,
        })
    }

    pub(crate) async fn begin_shutdown(&self) {
        // Nested workflow drivers own durable cleanup and must receive cancellation even if their
        // process-owned parent peer vanished before forwarding its per-call token.
        self.workflow_dispatch_shutdown.cancel();
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(BrokerCommand::BeginShutdown { response_tx })
            .is_ok()
        {
            let _ = response_rx.await;
        }
    }

    pub(crate) async fn finish_shutdown(&self) {
        self.workflow_dispatch_shutdown.cancel();
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(BrokerCommand::FinishShutdown { response_tx })
            .is_ok()
        {
            let _ = response_rx.await;
        }
        let task = match self.dispatcher_task.lock() {
            Ok(mut task) => task.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        // The broker task has closed every cell dispatcher. Because those dispatchers and the
        // nested drivers they create share one tracker, at least one tracked dispatcher remains
        // alive until any last queued driver is registered. Closing and waiting here therefore
        // cannot race a late spawn, and service shutdown does not return before nested cleanup.
        self.dispatch_tasks.close();
        self.dispatch_tasks.wait().await;
    }
}

impl Drop for CodeModeDispatchBroker {
    fn drop(&mut self) {
        self.workflow_dispatch_shutdown.cancel();
        self.dispatch_tasks.close();
        self.dispatcher_shutdown.cancel();
    }
}

pub(crate) struct PendingCellExecution {
    attempt_id: Option<u64>,
    command_tx: mpsc::UnboundedSender<BrokerCommand>,
}

impl PendingCellExecution {
    pub(crate) async fn bind(mut self, cell_id: CellId) -> Result<(), String> {
        let attempt_id = self
            .attempt_id
            .take()
            .ok_or_else(|| "code mode cell execution attempt was already consumed".to_string())?;
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(BrokerCommand::BindCell {
                attempt_id,
                cell_id,
                response_tx,
            })
            .map_err(|_| "code mode dispatch broker is unavailable".to_string())?;
        response_rx
            .await
            .map_err(|_| "code mode dispatch broker stopped".to_string())?
    }
}

impl Drop for PendingCellExecution {
    fn drop(&mut self) {
        if let Some(attempt_id) = self.attempt_id.take() {
            let _ = self
                .command_tx
                .send(BrokerCommand::AbortCellExecution { attempt_id });
        }
    }
}

pub(super) struct CoreTurnHostFactory {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    scheduler_max_threads: Option<usize>,
}

impl CoreTurnHostFactory {
    pub(super) fn bind_cell(&self) -> Arc<CoreTurnHost> {
        Arc::new(CoreTurnHost {
            exec: self.exec.clone(),
            tool_runtime: self.tool_runtime.clone(),
            // A scheduler is cell/run scoped, not sampling-worker scoped. Two workflow cells
            // advertised by one turn therefore have independent concurrency and lifetime caps.
            scheduler: new_workflow_scheduler(self.scheduler_max_threads),
        })
    }
}

pub(super) fn new_workflow_scheduler(scheduler_max_threads: Option<usize>) -> WorkflowScheduler {
    WorkflowScheduler::new(scheduler_max_threads)
}

#[cfg(test)]
pub(super) struct TestTurnHostFactory {
    pub(super) owner_id: String,
    pub(super) records: Arc<Mutex<Vec<TestDispatchRecord>>>,
    pub(super) schedulers: Arc<Mutex<Vec<WorkflowScheduler>>>,
}

#[cfg(test)]
impl TestTurnHostFactory {
    pub(super) fn bind_cell(&self) -> Arc<TestBoundHost> {
        let scheduler = new_workflow_scheduler(Some(4));
        match self.schedulers.lock() {
            Ok(mut schedulers) => schedulers.push(scheduler.clone()),
            Err(poisoned) => poisoned.into_inner().push(scheduler.clone()),
        }
        Arc::new(TestBoundHost {
            owner_id: self.owner_id.clone(),
            records: Arc::clone(&self.records),
            _scheduler: scheduler,
        })
    }
}

#[cfg(test)]
pub(super) struct TestBoundHost {
    pub(super) owner_id: String,
    pub(super) records: Arc<Mutex<Vec<TestDispatchRecord>>>,
    pub(super) _scheduler: WorkflowScheduler,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TestDispatchRecord {
    pub(super) owner_id: String,
    pub(super) cell_id: CellId,
    pub(super) text: String,
}

pub(crate) struct CodeModeDispatchWorker {
    pub(super) command_tx: mpsc::UnboundedSender<BrokerCommand>,
    pub(super) owner_id: String,
    pub(super) generation: u64,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(BrokerCommand::UnregisterTurn {
            owner_id: self.owner_id.clone(),
            generation: self.generation,
        });
    }
}
