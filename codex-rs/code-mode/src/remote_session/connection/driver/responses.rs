use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::RequestId;
use tokio::sync::oneshot;

use super::ConnectionDriver;
use super::cell_ids::public_runtime_response;
use super::cell_ids::public_wait_outcome;
use super::cell_ids::runtime_response_cell_id;
use super::cell_ids::validate_host_cell_ids;
use super::cell_ids::wait_outcome_cell_id;
use super::output_admission::AdmissionOutcome;
use super::output_admission::RemoteOutputAdmission;
use super::output_admission::ResponseDelivery;
use super::request_tracker::CancellationAction;
use super::session_registry::CellAdmissionError;
use super::types::DeliveredExecute;
use super::types::InitialResponse;
use super::types::PendingRequest;
use super::types::RemoteSession;
use super::types::UnclaimedExecute;

impl ConnectionDriver {
    pub(super) fn flush_deferred_waits(&mut self) -> bool {
        let mut deferred = self.requests.take_deferred_waits();
        while let Some(wait) = deferred.pop_front() {
            if wait.caller_cancellation.is_cancelled() {
                let _ = wait
                    .response_tx
                    .send(Err("code-mode request cancelled".to_string()));
                continue;
            }
            if self
                .requests
                .has_cancelled_wait(&wait.session, &wait.request.cell_id)
            {
                self.requests.push_deferred_wait(wait);
                continue;
            }
            if !self.start_wait(
                wait.session,
                wait.public_id,
                wait.request,
                wait.output_admission,
                wait.caller_cancellation,
                wait.response_tx,
            ) {
                for wait in deferred {
                    let reason = wait
                        .output_admission
                        .visible_connection_failure("code-mode host connection closed".to_string());
                    let _ = wait.response_tx.send(Err(reason));
                }
                return false;
            }
        }
        true
    }

    pub(super) fn handle_host_message(&mut self, message: HostToClient) -> bool {
        if let Err(err) = validate_host_cell_ids(&message) {
            self.fail(err.to_string());
            return false;
        }
        match message {
            HostToClient::Response { id, result } => {
                self.complete_request(id, result.into_result())
            }
            HostToClient::InitialResponse { id, result } => {
                self.complete_initial_response(id, result.into_result())
            }
            HostToClient::DelegateRequest {
                id,
                session_id,
                request,
            } => self.start_delegate(id, session_id, request),
            HostToClient::CancelDelegateRequest { id } => {
                self.delegates.cancel(id);
                true
            }
            HostToClient::CellClosed {
                session_id,
                cell_id,
            } => self.close_cell(session_id, cell_id),
            HostToClient::HostHello(_) | HostToClient::HandshakeRejected { .. } => {
                self.fail("code-mode host sent a second handshake response".to_string());
                false
            }
        }
    }

    fn complete_request(&mut self, id: RequestId, result: Result<HostResponse, String>) -> bool {
        let Some(pending) = self.requests.remove_pending(id) else {
            self.fail(format!("code-mode host returned unknown request ID {id:?}"));
            return false;
        };
        match pending {
            PendingRequest::OpenSession {
                session,
                delegate,
                cleanup,
                cancellation,
                response_tx,
            } => match result {
                Ok(HostResponse::SessionReady { session_id }) if session_id == session.id => {
                    let abandoned = cancellation.is_cancelled() || response_tx.is_closed();
                    self.sessions
                        .insert_ready(session.clone(), delegate, cleanup);
                    if abandoned || response_tx.send(Ok(())).is_err() {
                        return self.shutdown_abandoned_session(session);
                    }
                }
                Ok(_) => {
                    let reason =
                        "code-mode host returned an invalid open-session response".to_string();
                    let _ = response_tx.send(Err(reason.clone()));
                    self.fail(reason);
                    return false;
                }
                Err(err) => {
                    let _ = response_tx.send(Err(err));
                }
            },
            PendingRequest::Execute {
                session,
                response_tx,
                initial_response_tx,
                initial_response_rx,
                output_admission,
                cancellation,
            } => match result {
                Ok(HostResponse::ExecutionStarted { cell_id }) => {
                    // The host owns a checked, never-reused ID sequence. Retain only live
                    // IDs so client memory scales with concurrency, not session lifetime.
                    let remote_cell_id = cell_id.clone();
                    let public_id = match self.sessions.admit_cell(
                        &session,
                        cell_id,
                        output_admission.clone(),
                        &self.workflow_cell_ids,
                    ) {
                        Ok(public_id) => public_id,
                        Err(CellAdmissionError::MissingSession) => {
                            let (reason, admission) = output_admission
                                .admit_error("code-mode session closed during execute".to_string());
                            return self.deliver_admitted(response_tx, Err(reason), admission);
                        }
                        Err(CellAdmissionError::DuplicateCell) => {
                            let reason = format!(
                                "code-mode host reused live cell {} in session {}",
                                remote_cell_id.as_str(),
                                session.id
                            );
                            return self.fail_admitted(response_tx, reason, &output_admission);
                        }
                    };
                    self.requests.insert_initial_response(
                        id,
                        InitialResponse {
                            public_id: public_id.clone(),
                            cell_id: remote_cell_id.clone(),
                            output_admission: output_admission.clone(),
                            response_tx: initial_response_tx,
                        },
                    );
                    let started =
                        StartedCell::from_result_receiver(public_id.clone(), initial_response_rx);
                    let execute = UnclaimedExecute {
                        session,
                        public_id,
                        cell_id: remote_cell_id,
                        output_admission,
                        cancellation,
                    };
                    if execute.cancellation.is_cancelled() || response_tx.is_closed() {
                        return self.terminate_abandoned(execute);
                    }
                    let delivered = DeliveredExecute {
                        request_id: id,
                        started,
                    };
                    if response_tx.send(Ok(delivered)).is_err() {
                        return self.terminate_abandoned(execute);
                    }
                    self.requests.insert_unclaimed_execute(id, execute);
                }
                Ok(_) => {
                    let reason = "code-mode host returned an invalid execute response".to_string();
                    return self.fail_admitted(response_tx, reason, &output_admission);
                }
                Err(err) => {
                    let (err, admission) = output_admission.admit_error(err);
                    return self.deliver_admitted(response_tx, Err(err), admission);
                }
            },
            PendingRequest::Wait {
                session: _,
                public_id,
                cell_id,
                output_admission,
                response_tx,
                ..
            } => {
                let (response, admission) = match result {
                    Ok(HostResponse::WaitCompleted { outcome }) => {
                        if wait_outcome_cell_id(&outcome) != &cell_id {
                            let reason = format!(
                                "code-mode host returned cell {} for request targeting {}",
                                wait_outcome_cell_id(&outcome).as_str(),
                                cell_id.as_str()
                            );
                            return self.fail_admitted(response_tx, reason, &output_admission);
                        }
                        let mut outcome: WaitOutcome = outcome.into();
                        let (response, delivery) = match &mut outcome {
                            WaitOutcome::LiveCell(response) => {
                                (response, ResponseDelivery::Observer)
                            }
                            WaitOutcome::MissingCell(response) => {
                                (response, ResponseDelivery::Uncorrelated)
                            }
                        };
                        let admission = output_admission.admit_response(response, delivery);
                        (Ok(public_wait_outcome(public_id, outcome)), admission)
                    }
                    Ok(_) => {
                        let reason = "code-mode host returned an invalid cell response".to_string();
                        return self.fail_admitted(response_tx, reason, &output_admission);
                    }
                    Err(err) => {
                        let (err, admission) = output_admission.admit_error(err);
                        (Err(err), admission)
                    }
                };
                return self.deliver_admitted(response_tx, response, admission);
            }
            PendingRequest::Terminate {
                public_id,
                cell_id,
                output_admission,
                response_tx,
            } => {
                let (response, admission) = match result {
                    Ok(HostResponse::WaitCompleted { outcome }) => {
                        if wait_outcome_cell_id(&outcome) != &cell_id {
                            let reason = format!(
                                "code-mode host returned cell {} for request targeting {}",
                                wait_outcome_cell_id(&outcome).as_str(),
                                cell_id.as_str()
                            );
                            return self.fail_admitted(response_tx, reason, &output_admission);
                        }
                        let mut outcome: WaitOutcome = outcome.into();
                        let (response, delivery) = match &mut outcome {
                            WaitOutcome::LiveCell(response) => {
                                (response, ResponseDelivery::Terminate)
                            }
                            WaitOutcome::MissingCell(response) => {
                                (response, ResponseDelivery::Uncorrelated)
                            }
                        };
                        let admission = output_admission.admit_response(response, delivery);
                        (Ok(public_wait_outcome(public_id, outcome)), admission)
                    }
                    Ok(_) => {
                        let reason = "code-mode host returned an invalid cell response".to_string();
                        return self.fail_admitted(response_tx, reason, &output_admission);
                    }
                    Err(err) => {
                        let (err, admission) = output_admission.admit_error(err);
                        (Err(err), admission)
                    }
                };
                return self.deliver_admitted(response_tx, response, admission);
            }
            PendingRequest::ShutdownSession {
                session,
                response_tx,
            } => match result {
                Ok(HostResponse::SessionClosed { session_id }) if session_id == session.id => {
                    let effects = self.close_session_locally(&session.id);
                    if !self.apply_delegate_effects(effects) {
                        return false;
                    }
                    let _ = response_tx.send(Ok(()));
                }
                Ok(_) => {
                    let err = "code-mode host returned an invalid shutdown response".to_string();
                    let _ = response_tx.send(Err(err.clone()));
                    self.fail(err);
                    return false;
                }
                Err(err) => {
                    let _ = response_tx.send(Err(err.clone()));
                    self.fail(err);
                    return false;
                }
            },
        }
        true
    }

    pub(super) fn cancel_dropped_callers(&mut self) -> bool {
        for action in self.requests.collect_cancellations() {
            if !self.apply_cancellation(action) {
                return false;
            }
        }
        true
    }

    pub(super) fn cancel_request(&mut self, id: RequestId) -> bool {
        self.requests
            .mark_cancelled(id)
            .is_none_or(|action| self.apply_cancellation(action))
    }

    fn apply_cancellation(&mut self, action: CancellationAction) -> bool {
        match action {
            CancellationAction::Send(id) => self.send_cancel_request(id),
            CancellationAction::Terminate {
                request_id,
                execute,
            } => {
                if !self.send_cancel_request(request_id) {
                    return false;
                }
                self.terminate_abandoned(execute)
            }
        }
    }

    fn send_cancel_request(&mut self, id: RequestId) -> bool {
        let frame = match EncodedFrame::encode(&ClientToHost::CancelRequest { id }) {
            Ok(frame) => frame,
            Err(err) => {
                self.fail(format!(
                    "failed to encode code-mode cancellation request: {err}"
                ));
                return false;
            }
        };
        self.queue_frame(frame)
    }

    fn shutdown_abandoned_session(&mut self, session: RemoteSession) -> bool {
        let Some(should_shutdown) = self.sessions.begin_abandoned_shutdown(&session.id) else {
            self.fail(format!(
                "code-mode host committed abandoned session {} without local state",
                session.id
            ));
            return false;
        };
        if !should_shutdown {
            return true;
        }
        let (response_tx, response_rx) = oneshot::channel();
        drop(response_rx);
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

    fn terminate_abandoned(&mut self, execute: UnclaimedExecute) -> bool {
        let UnclaimedExecute {
            session,
            public_id,
            cell_id,
            output_admission,
            ..
        } = execute;
        let Some(is_closing) = self.sessions.is_closing(&session.id) else {
            self.fail(format!(
                "code-mode host admitted an abandoned cell in unknown session {}",
                session.id
            ));
            return false;
        };
        if is_closing {
            return true;
        }
        let (response_tx, response_rx) = oneshot::channel();
        drop(response_rx);
        self.send_request(
            HostRequest::Terminate {
                session_id: session.id,
                cell_id: cell_id.clone(),
            },
            PendingRequest::Terminate {
                public_id,
                cell_id,
                output_admission,
                response_tx,
            },
        )
    }

    fn complete_initial_response(
        &mut self,
        id: RequestId,
        result: Result<codex_code_mode_protocol::host::WireRuntimeResponse, String>,
    ) -> bool {
        let Some(initial) = self.requests.remove_initial_response(id) else {
            self.fail(format!(
                "code-mode host returned initial response for unknown request ID {id:?}"
            ));
            return false;
        };
        let (response, admission) = match result {
            Ok(response) if runtime_response_cell_id(&response) == &initial.cell_id => {
                let mut response: RuntimeResponse = response.into();
                let admission = initial
                    .output_admission
                    .admit_response(&mut response, ResponseDelivery::Observer);
                (
                    Ok(public_runtime_response(initial.public_id, response)),
                    admission,
                )
            }
            Ok(response) => {
                let reason = format!(
                    "code-mode host returned initial response for cell {} instead of {}",
                    runtime_response_cell_id(&response).as_str(),
                    initial.cell_id.as_str()
                );
                return self.fail_admitted(initial.response_tx, reason, &initial.output_admission);
            }
            Err(err) => {
                let (err, admission) = initial.output_admission.admit_error(err);
                (Err(err), admission)
            }
        };
        self.deliver_admitted(initial.response_tx, response, admission)
    }

    fn deliver_admitted<T>(
        &mut self,
        response_tx: oneshot::Sender<Result<T, String>>,
        response: Result<T, String>,
        admission: AdmissionOutcome,
    ) -> bool {
        let _ = response_tx.send(response);
        match admission {
            AdmissionOutcome::Admitted => true,
            AdmissionOutcome::ExecutionFailed => {
                self.fail(SAVED_WORKFLOW_EXECUTION_FAILED.to_string());
                false
            }
            AdmissionOutcome::Rejected => {
                self.fail(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string());
                false
            }
        }
    }

    fn fail_admitted<T>(
        &mut self,
        response_tx: oneshot::Sender<Result<T, String>>,
        reason: String,
        output_admission: &RemoteOutputAdmission,
    ) -> bool {
        let (visible_reason, _) = output_admission.admit_fatal_error(reason);
        let _ = response_tx.send(Err(visible_reason.clone()));
        self.fail(visible_reason);
        false
    }
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
