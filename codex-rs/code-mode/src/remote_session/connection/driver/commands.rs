use std::sync::Arc;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::WireExecuteRequest;
use codex_code_mode_protocol::host::WireWaitRequest;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::ConnectionDriver;
use super::output_admission::RemoteOutputAdmission;
use super::types::CancellableRequest;
use super::types::DeferredWait;
use super::types::DeliveredExecute;
use super::types::DriverCommand;
use super::types::PendingRequest;
use super::types::RemoteSession;
use super::workflow_cell_ids::CellProvenance;
use super::workflow_cell_ids::ExpectedCellIdentity;
use super::workflow_cell_ids::ResolvedCellId;
use super::workflow_cell_ids::WorkflowCellIdAllocationError;

impl ConnectionDriver {
    pub(super) fn handle_command(&mut self, command: DriverCommand) -> bool {
        match command {
            DriverCommand::OpenSession {
                session,
                delegate,
                cleanup,
                caller_cancellation,
                response_tx,
            } => self.open_session(session, delegate, cleanup, caller_cancellation, response_tx),
            DriverCommand::Execute {
                session,
                request,
                caller_cancellation,
                response_tx,
            } => self.execute(session, request, caller_cancellation, response_tx),
            DriverCommand::Wait {
                session,
                request,
                caller_cancellation,
                response_tx,
            } => self.wait(session, request, caller_cancellation, response_tx),
            DriverCommand::Terminate {
                session,
                cell_id,
                response_tx,
            } => self.terminate(session, cell_id, response_tx),
            DriverCommand::ShutdownSession {
                session,
                response_tx,
            } => self.shutdown_session(session, response_tx),
        }
    }

    fn open_session(
        &mut self,
        session: RemoteSession,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        cleanup: super::cleanup::SessionCleanup,
        caller_cancellation: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    ) -> bool {
        if self.sessions.contains(&session.id) || self.requests.contains_pending_open(&session) {
            let _ = response_tx.send(Err(format!(
                "code-mode session {} is already open",
                session.id
            )));
            return true;
        }
        let request_id = match self.requests.allocate_id() {
            Ok(id) => id,
            Err(err) => {
                let _ = response_tx.send(Err(err));
                return false;
            }
        };
        let message = ClientToHost::Request {
            id: request_id,
            request: HostRequest::OpenSession {
                session_id: session.id.clone(),
            },
        };
        let frame = match EncodedFrame::encode(&message) {
            Ok(frame) => frame,
            Err(err) => {
                let _ = response_tx.send(Err(format!(
                    "failed to encode code-mode open-session request: {err}"
                )));
                return true;
            }
        };
        let cancellation = CancellableRequest::new(caller_cancellation);
        self.requests.insert_pending(
            request_id,
            PendingRequest::OpenSession {
                session,
                delegate,
                cleanup,
                cancellation,
                response_tx,
            },
            &self.event_tx,
        );
        self.queue_frame(frame)
    }

    fn execute(
        &mut self,
        session: RemoteSession,
        request: ExecuteRequest,
        caller_cancellation: CancellationToken,
        response_tx: oneshot::Sender<Result<DeliveredExecute, String>>,
    ) -> bool {
        if let Err(err) = self.sessions.require_ready(&session) {
            let _ = response_tx.send(Err(err));
            return true;
        }
        let expected_cell_identity = match request.output_policy {
            ExecuteOutputPolicy::Ordinary => ExpectedCellIdentity::HostAllocated,
            ExecuteOutputPolicy::SavedWorkflow => {
                match self.workflow_cell_ids.allocate_saved_cell_id() {
                    Ok(cell_id) => ExpectedCellIdentity::SavedWorkflow(cell_id),
                    Err(err) => {
                        let keep_running =
                            matches!(err, WorkflowCellIdAllocationError::Unavailable);
                        let _ = response_tx.send(Err(err.to_string()));
                        return keep_running;
                    }
                }
            }
        };
        let output_admission = RemoteOutputAdmission::with_terminal_echo_budget(
            request.output_policy,
            self.terminal_echo_budget.clone(),
        );
        let request = match &expected_cell_identity {
            ExpectedCellIdentity::HostAllocated => {
                WireExecuteRequest::try_from_domain(request, self.capabilities.selected())
            }
            ExpectedCellIdentity::SavedWorkflow(cell_id) => {
                WireExecuteRequest::try_from_domain_with_workflow_cell_id(
                    request,
                    self.capabilities.selected(),
                    cell_id.clone(),
                )
            }
        };
        let request = match request {
            Ok(request) => request,
            Err(err) => {
                let _ = response_tx.send(Err(format!(
                    "failed to encode code-mode execute request: {err}"
                )));
                return true;
            }
        };
        let request_id = match self.requests.allocate_id() {
            Ok(id) => id,
            Err(err) => {
                let _ = response_tx.send(Err(err));
                return false;
            }
        };
        let message = ClientToHost::Request {
            id: request_id,
            request: HostRequest::Execute {
                session_id: session.id.clone(),
                request,
            },
        };
        let frame = match EncodedFrame::encode(&message) {
            Ok(frame) => frame,
            Err(err) => {
                let _ = response_tx.send(Err(format!(
                    "code-mode execute request exceeds the IPC frame limit: {err}"
                )));
                return true;
            }
        };
        let (initial_response_tx, initial_response_rx) = oneshot::channel();
        let cancellation = CancellableRequest::new(caller_cancellation);
        self.requests.insert_pending(
            request_id,
            PendingRequest::Execute {
                session,
                response_tx,
                initial_response_tx,
                initial_response_rx,
                expected_cell_identity,
                output_admission,
                cancellation,
            },
            &self.event_tx,
        );
        self.queue_frame(frame)
    }

    fn wait(
        &mut self,
        session: RemoteSession,
        request: WaitRequest,
        caller_cancellation: CancellationToken,
        response_tx: oneshot::Sender<Result<WaitOutcome, String>>,
    ) -> bool {
        if let Err(err) = self.sessions.require_ready(&session) {
            let _ = response_tx.send(Err(err));
            return true;
        }
        let public_id = request.cell_id.clone();
        let resolved = match self.workflow_cell_ids.remote_cell_id(&session, &public_id) {
            Ok(resolved) => resolved,
            Err(err) => {
                let _ = response_tx.send(Err(err));
                return true;
            }
        };
        let ResolvedCellId {
            wire_id,
            provenance,
        } = resolved;
        let output_admission = match (self.sessions.cell_output(&session, &wire_id), provenance) {
            (Ok(Some(output_admission)), _) => output_admission,
            (Ok(None), CellProvenance::SavedWorkflow) => {
                let _ = response_tx.send(Ok(missing_cell_outcome(public_id)));
                return true;
            }
            (Ok(None), CellProvenance::Ordinary) => {
                RemoteOutputAdmission::new(ExecuteOutputPolicy::Ordinary)
            }
            (Err(err), _) => {
                let _ = response_tx.send(Err(err));
                return true;
            }
        };
        let request = WireWaitRequest {
            cell_id: wire_id,
            yield_time_ms: request.yield_time_ms,
        };
        if self.requests.has_cancelled_wait(&session, &request.cell_id) {
            self.requests.push_deferred_wait(DeferredWait {
                session,
                public_id,
                request,
                output_admission,
                caller_cancellation,
                response_tx,
            });
            return true;
        }
        self.start_wait(
            session,
            public_id,
            request,
            output_admission,
            caller_cancellation,
            response_tx,
        )
    }

    pub(super) fn start_wait(
        &mut self,
        session: RemoteSession,
        public_id: CellId,
        request: WireWaitRequest,
        output_admission: RemoteOutputAdmission,
        caller_cancellation: CancellationToken,
        response_tx: oneshot::Sender<Result<WaitOutcome, String>>,
    ) -> bool {
        let cell_id = request.cell_id.clone();
        self.send_request(
            HostRequest::Wait {
                session_id: session.id.clone(),
                request,
            },
            PendingRequest::Wait {
                session,
                public_id,
                cell_id,
                output_admission,
                cancellation: CancellableRequest::new(caller_cancellation),
                response_tx,
            },
        )
    }

    fn terminate(
        &mut self,
        session: RemoteSession,
        cell_id: CellId,
        response_tx: oneshot::Sender<Result<WaitOutcome, String>>,
    ) -> bool {
        if let Err(err) = self.sessions.require_ready(&session) {
            let _ = response_tx.send(Err(err));
            return true;
        }
        let public_id = cell_id.clone();
        let resolved = match self.workflow_cell_ids.remote_cell_id(&session, &cell_id) {
            Ok(resolved) => resolved,
            Err(err) => {
                let _ = response_tx.send(Err(err));
                return true;
            }
        };
        let ResolvedCellId {
            wire_id: cell_id,
            provenance,
        } = resolved;
        let output_admission = match (self.sessions.cell_output(&session, &cell_id), provenance) {
            (Ok(Some(output_admission)), _) => output_admission,
            (Ok(None), CellProvenance::SavedWorkflow) => {
                let _ = response_tx.send(Ok(missing_cell_outcome(public_id)));
                return true;
            }
            (Ok(None), CellProvenance::Ordinary) => {
                RemoteOutputAdmission::new(ExecuteOutputPolicy::Ordinary)
            }
            (Err(err), _) => {
                let _ = response_tx.send(Err(err));
                return true;
            }
        };
        let pending_cell_id = cell_id.clone();
        self.send_request(
            HostRequest::Terminate {
                session_id: session.id,
                cell_id,
            },
            PendingRequest::Terminate {
                public_id,
                cell_id: pending_cell_id,
                output_admission,
                response_tx,
            },
        )
    }

    fn shutdown_session(
        &mut self,
        session: RemoteSession,
        response_tx: oneshot::Sender<Result<(), String>>,
    ) -> bool {
        if let Err(err) = self.sessions.begin_shutdown(&session) {
            let _ = response_tx.send(Err(err));
            return true;
        }
        self.send_request(
            HostRequest::ShutdownSession {
                session_id: session.id.clone(),
            },
            PendingRequest::ShutdownSession {
                session,
                response_tx,
            },
        )
    }

    pub(super) fn send_request(&mut self, request: HostRequest, pending: PendingRequest) -> bool {
        let request_id = match self.requests.allocate_id() {
            Ok(id) => id,
            Err(err) => {
                pending.fail(err);
                return false;
            }
        };
        let message = ClientToHost::Request {
            id: request_id,
            request,
        };
        let frame = match EncodedFrame::encode(&message) {
            Ok(frame) => frame,
            Err(err) => {
                pending.fail(format!(
                    "code-mode request exceeds the IPC frame limit: {err}"
                ));
                return true;
            }
        };
        self.requests
            .insert_pending(request_id, pending, &self.event_tx);
        self.queue_frame(frame)
    }
}

fn missing_cell_outcome(cell_id: CellId) -> WaitOutcome {
    WaitOutcome::MissingCell(RuntimeResponse::Result {
        error_text: Some(format!("exec cell {cell_id} not found")),
        cell_id,
        content_items: Vec::new(),
    })
}
