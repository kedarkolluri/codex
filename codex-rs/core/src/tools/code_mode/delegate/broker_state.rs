use super::*;

#[derive(Clone)]
pub(super) enum DispatchContext {
    Core(Arc<CoreTurnHostFactory>),
    #[cfg(test)]
    Test(Arc<TestTurnHostFactory>),
    #[allow(dead_code, reason = "used by event-disabled hermetic workflow tests")]
    Disabled,
}

impl DispatchContext {
    pub(super) fn bind_cell(&self) -> BoundDispatchHost {
        match self {
            Self::Core(factory) => BoundDispatchHost::Core(factory.bind_cell()),
            #[cfg(test)]
            Self::Test(factory) => BoundDispatchHost::Test(factory.bind_cell()),
            Self::Disabled => BoundDispatchHost::Disabled,
        }
    }
}

#[derive(Clone)]
pub(super) enum BoundDispatchHost {
    Core(Arc<CoreTurnHost>),
    #[cfg(test)]
    Test(Arc<TestBoundHost>),
    Disabled,
}

struct TurnRegistration {
    generation: u64,
    context: DispatchContext,
}

struct CellExecutionAttempt {
    context: DispatchContext,
}

struct CellRoute {
    context: DispatchContext,
    command_tx: mpsc::UnboundedSender<CellDispatchCommand>,
    tasks: TaskTracker,
    dispatcher_done: CancellationToken,
}

pub(super) enum CellDispatchCommand {
    Dispatch(Box<DispatchMessage>),
    Ready,
    Close(String),
}

pub(super) enum BrokerCommand {
    RegisterTurn {
        owner_id: String,
        context: DispatchContext,
        response_tx: oneshot::Sender<Result<u64, String>>,
    },
    UnregisterTurn {
        owner_id: String,
        generation: u64,
    },
    BeginCellExecution {
        origin: CodeModeDispatchOrigin,
        response_tx: oneshot::Sender<Result<u64, String>>,
    },
    BindCell {
        attempt_id: u64,
        cell_id: CellId,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    AbortCellExecution {
        attempt_id: u64,
    },
    MarkCellReady {
        cell_id: CellId,
    },
    CloseCell {
        cell_id: CellId,
    },
    CloseCellAndWait {
        cell_id: CellId,
        response_tx: oneshot::Sender<()>,
    },
    Dispatch(Box<DispatchMessage>),
    BeginShutdown {
        response_tx: oneshot::Sender<()>,
    },
    FinishShutdown {
        response_tx: oneshot::Sender<()>,
    },
}

struct DispatchBrokerState {
    registrations: HashMap<String, TurnRegistration>,
    attempts: HashMap<u64, CellExecutionAttempt>,
    cells: HashMap<CellId, CellRoute>,
    unbound_messages: HashMap<CellId, VecDeque<Box<DispatchMessage>>>,
    unbound_message_count: usize,
    closed_cells: HashSet<CellId>,
    closed_cell_order: VecDeque<CellId>,
    next_generation: u64,
    next_attempt_id: u64,
    shutting_down: bool,
    workflow_run_ledger: Arc<WorkflowRunLedger>,
    dispatch_tasks: TaskTracker,
    workflow_dispatch_shutdown: CancellationToken,
}

impl DispatchBrokerState {
    fn new(
        workflow_run_ledger: Arc<WorkflowRunLedger>,
        dispatch_tasks: TaskTracker,
        workflow_dispatch_shutdown: CancellationToken,
    ) -> Self {
        Self {
            registrations: HashMap::new(),
            attempts: HashMap::new(),
            cells: HashMap::new(),
            unbound_messages: HashMap::new(),
            unbound_message_count: 0,
            closed_cells: HashSet::new(),
            closed_cell_order: VecDeque::new(),
            next_generation: 0,
            next_attempt_id: 0,
            shutting_down: false,
            workflow_run_ledger,
            dispatch_tasks,
            workflow_dispatch_shutdown,
        }
    }

    fn next_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        generation
    }

    fn register_turn(&mut self, owner_id: String, context: DispatchContext) -> Result<u64, String> {
        if self.shutting_down {
            return Err("code mode dispatch broker is shutting down".to_string());
        }
        let generation = self.next_generation();
        self.registrations.insert(
            owner_id,
            TurnRegistration {
                generation,
                context,
            },
        );
        Ok(generation)
    }

    fn unregister_turn(&mut self, owner_id: &str, generation: u64) {
        if self
            .registrations
            .get(owner_id)
            .is_some_and(|registration| registration.generation == generation)
        {
            self.registrations.remove(owner_id);
        }
    }

    fn begin_cell_execution(&mut self, origin: CodeModeDispatchOrigin) -> Result<u64, String> {
        if self.shutting_down {
            return Err("code mode dispatch broker is shutting down".to_string());
        }
        let context = match origin {
            CodeModeDispatchOrigin::Turn(owner_id) => self
                .registrations
                .get(&owner_id)
                .map(|registration| registration.context.clone())
                .ok_or_else(|| {
                    format!("code mode turn dispatch owner `{owner_id}` is not registered")
                })?,
            CodeModeDispatchOrigin::ParentCell(cell_id) => self
                .cells
                .get(&cell_id)
                .map(|route| route.context.clone())
                .ok_or_else(|| {
                    format!("code mode parent cell `{cell_id}` is not bound for dispatch")
                })?,
            #[cfg(test)]
            CodeModeDispatchOrigin::Disabled => DispatchContext::Disabled,
        };
        let attempt_id = self.next_attempt_id;
        self.next_attempt_id = self.next_attempt_id.saturating_add(1);
        self.attempts
            .insert(attempt_id, CellExecutionAttempt { context });
        Ok(attempt_id)
    }

    fn bind_cell(&mut self, attempt_id: u64, cell_id: CellId) -> Result<(), String> {
        let attempt = self.attempts.remove(&attempt_id).ok_or_else(|| {
            format!("code mode cell execution attempt `{attempt_id}` is not active")
        })?;
        if self.closed_cells.contains(&cell_id) {
            self.fail_unbound_for_cell(
                &cell_id,
                format!("code mode cell `{cell_id}` closed before dispatch binding"),
            );
            self.fail_orphaned_unbound_messages();
            return Ok(());
        }
        if self.cells.contains_key(&cell_id) {
            self.fail_orphaned_unbound_messages();
            return Err(format!(
                "code mode cell `{cell_id}` is already bound for dispatch"
            ));
        }
        let host = attempt.context.bind_cell();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let cell_tasks = TaskTracker::new();
        let dispatcher_done = CancellationToken::new();
        self.dispatch_tasks.spawn(run_cell_dispatcher(
            host,
            command_rx,
            Arc::clone(&self.workflow_run_ledger),
            self.dispatch_tasks.clone(),
            cell_tasks.clone(),
            self.workflow_dispatch_shutdown.clone(),
            dispatcher_done.clone(),
        ));
        self.cells.insert(
            cell_id.clone(),
            CellRoute {
                context: attempt.context,
                command_tx: command_tx.clone(),
                tasks: cell_tasks,
                dispatcher_done,
            },
        );
        if let Some(messages) = self.unbound_messages.remove(&cell_id) {
            self.unbound_message_count = self.unbound_message_count.saturating_sub(messages.len());
            for message in messages {
                if let Err(error) = command_tx.send(CellDispatchCommand::Dispatch(message))
                    && let CellDispatchCommand::Dispatch(message) = error.0
                {
                    message.fail(format!("code mode cell `{cell_id}` dispatcher stopped"));
                }
            }
        }
        self.fail_orphaned_unbound_messages();
        Ok(())
    }

    fn abort_cell_execution(&mut self, attempt_id: u64) {
        self.attempts.remove(&attempt_id);
        self.fail_orphaned_unbound_messages();
    }

    fn mark_cell_ready(&mut self, cell_id: &CellId) {
        if let Some(route) = self.cells.get(cell_id) {
            let _ = route.command_tx.send(CellDispatchCommand::Ready);
        }
    }

    fn close_cell(&mut self, cell_id: &CellId) -> Option<CellRoute> {
        let route = self.cells.remove(cell_id);
        if let Some(route) = route.as_ref() {
            let _ = route.command_tx.send(CellDispatchCommand::Close(format!(
                "code mode cell `{cell_id}` is closed"
            )));
        }
        self.fail_unbound_for_cell(
            cell_id,
            format!("code mode cell `{cell_id}` closed before dispatch binding"),
        );
        self.remember_closed_cell(cell_id.clone());
        route
    }

    fn remember_closed_cell(&mut self, cell_id: CellId) {
        if self.closed_cells.insert(cell_id.clone()) {
            self.closed_cell_order.push_back(cell_id);
        }
        while self.closed_cell_order.len() > CLOSED_CELL_TOMBSTONE_CAP {
            if let Some(cell_id) = self.closed_cell_order.pop_front() {
                self.closed_cells.remove(&cell_id);
            }
        }
    }

    fn dispatch(&mut self, message: Box<DispatchMessage>) {
        let cell_id = message.cell_id().clone();
        if let Some(route) = self.cells.get(&cell_id) {
            if let Err(error) = route
                .command_tx
                .send(CellDispatchCommand::Dispatch(message))
                && let CellDispatchCommand::Dispatch(message) = error.0
            {
                message.fail(format!("code mode cell `{cell_id}` dispatcher stopped"));
            }
            return;
        }
        if self.closed_cells.contains(&cell_id) {
            message.fail(format!("code mode cell `{cell_id}` is closed"));
            return;
        }
        if self.attempts.is_empty() {
            message.fail(format!(
                "code mode cell `{cell_id}` is not bound for dispatch"
            ));
            return;
        }
        let per_cell_count = self.unbound_messages.get(&cell_id).map_or(0, VecDeque::len);
        if self.unbound_message_count >= MAX_UNBOUND_DISPATCH_MESSAGES
            || per_cell_count >= MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL
        {
            message.fail(format!(
                "code mode cell `{cell_id}` exceeded the pre-bind dispatch queue limit"
            ));
            return;
        }
        self.unbound_messages
            .entry(cell_id)
            .or_default()
            .push_back(message);
        self.unbound_message_count = self.unbound_message_count.saturating_add(1);
    }

    fn fail_unbound_for_cell(&mut self, cell_id: &CellId, reason: String) {
        if let Some(messages) = self.unbound_messages.remove(cell_id) {
            self.unbound_message_count = self.unbound_message_count.saturating_sub(messages.len());
            for message in messages {
                message.fail(reason.clone());
            }
        }
    }

    fn fail_orphaned_unbound_messages(&mut self) {
        if !self.attempts.is_empty() {
            return;
        }
        let messages = std::mem::take(&mut self.unbound_messages);
        self.unbound_message_count = 0;
        for (cell_id, messages) in messages {
            for message in messages {
                message.fail(format!(
                    "code mode cell `{cell_id}` was never bound for dispatch"
                ));
            }
        }
    }

    fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.workflow_dispatch_shutdown.cancel();
        self.attempts.clear();
        self.fail_orphaned_unbound_messages();
        self.registrations.clear();
    }

    fn finish_shutdown(&mut self) {
        self.begin_shutdown();
        let cells = std::mem::take(&mut self.cells);
        for (cell_id, route) in cells {
            let _ = route.command_tx.send(CellDispatchCommand::Close(format!(
                "code mode cell `{cell_id}` dispatcher is shutting down"
            )));
        }
    }
}

pub(super) async fn run_dispatch_broker(
    mut command_rx: mpsc::UnboundedReceiver<BrokerCommand>,
    workflow_run_ledger: Arc<WorkflowRunLedger>,
    shutdown: CancellationToken,
    dispatch_tasks: TaskTracker,
    workflow_dispatch_shutdown: CancellationToken,
) {
    let mut state = DispatchBrokerState::new(
        workflow_run_ledger,
        dispatch_tasks,
        workflow_dispatch_shutdown,
    );
    loop {
        let command = tokio::select! {
            _ = shutdown.cancelled() => {
                state.finish_shutdown();
                break;
            }
            command = command_rx.recv() => command,
        };
        let Some(command) = command else {
            state.finish_shutdown();
            break;
        };
        match command {
            BrokerCommand::RegisterTurn {
                owner_id,
                context,
                response_tx,
            } => {
                let _ = response_tx.send(state.register_turn(owner_id, context));
            }
            BrokerCommand::UnregisterTurn {
                owner_id,
                generation,
            } => state.unregister_turn(&owner_id, generation),
            BrokerCommand::BeginCellExecution {
                origin,
                response_tx,
            } => {
                let _ = response_tx.send(state.begin_cell_execution(origin));
            }
            BrokerCommand::BindCell {
                attempt_id,
                cell_id,
                response_tx,
            } => {
                let _ = response_tx.send(state.bind_cell(attempt_id, cell_id));
            }
            BrokerCommand::AbortCellExecution { attempt_id } => {
                state.abort_cell_execution(attempt_id);
            }
            BrokerCommand::MarkCellReady { cell_id } => state.mark_cell_ready(&cell_id),
            BrokerCommand::CloseCell { cell_id } => {
                state.close_cell(&cell_id);
            }
            BrokerCommand::CloseCellAndWait {
                cell_id,
                response_tx,
            } => {
                let Some(route) = state.close_cell(&cell_id) else {
                    let _ = response_tx.send(());
                    continue;
                };
                state.dispatch_tasks.spawn(async move {
                    route.dispatcher_done.cancelled().await;
                    route.tasks.close();
                    route.tasks.wait().await;
                    let _ = response_tx.send(());
                });
            }
            BrokerCommand::Dispatch(message) => state.dispatch(message),
            BrokerCommand::BeginShutdown { response_tx } => {
                state.begin_shutdown();
                let _ = response_tx.send(());
            }
            BrokerCommand::FinishShutdown { response_tx } => {
                state.finish_shutdown();
                let _ = response_tx.send(());
                break;
            }
        }
    }
    while let Ok(command) = command_rx.try_recv() {
        command.fail("code mode dispatch broker is stopped".to_string());
    }
}

impl BrokerCommand {
    pub(super) fn dispatch(message: DispatchMessage) -> Self {
        Self::Dispatch(Box::new(message))
    }

    fn fail(self, reason: String) {
        match self {
            Self::RegisterTurn { response_tx, .. } => {
                let _ = response_tx.send(Err(reason));
            }
            Self::BeginCellExecution { response_tx, .. } => {
                let _ = response_tx.send(Err(reason));
            }
            Self::BindCell { response_tx, .. } => {
                let _ = response_tx.send(Err(reason));
            }
            Self::Dispatch(message) => message.fail(reason),
            Self::BeginShutdown { response_tx }
            | Self::FinishShutdown { response_tx }
            | Self::CloseCellAndWait { response_tx, .. } => {
                let _ = response_tx.send(());
            }
            Self::UnregisterTurn { .. }
            | Self::AbortCellExecution { .. }
            | Self::MarkCellReady { .. }
            | Self::CloseCell { .. } => {}
        }
    }
}
