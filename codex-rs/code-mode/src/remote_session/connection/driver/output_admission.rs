use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WorkflowOutputBounds;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const MAX_UNMATCHED_TERMINAL_ECHOES: usize = 128;
const INVALID_TERMINAL_DELIVERY: &str =
    "code-mode host returned an invalid terminal delivery sequence";

#[derive(Clone)]
pub(super) enum RemoteOutputAdmission {
    Ordinary,
    SavedWorkflow(Arc<Mutex<SavedAdmissionState>>),
}

pub(super) enum SavedAdmissionState {
    Open(OpenSavedAdmission),
    ExecutionFailed,
    Rejected,
}

pub(super) struct OpenSavedAdmission {
    bounds: WorkflowOutputBounds,
    terminal_echo: TerminalEchoState,
    terminal_echo_budget: TerminalEchoBudget,
}

enum TerminalEchoState {
    Empty,
    Unmatched {
        role: ResponseDelivery,
        response: RuntimeResponse,
        _permit: OwnedSemaphorePermit,
    },
    Matched,
}

#[derive(Clone)]
pub(super) struct TerminalEchoBudget(Arc<Semaphore>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponseDelivery {
    Observer,
    Terminate,
    Uncorrelated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AdmissionOutcome {
    Admitted,
    ExecutionFailed,
    Rejected,
}

impl RemoteOutputAdmission {
    pub(super) fn new(output_policy: ExecuteOutputPolicy) -> Self {
        Self::with_terminal_echo_budget(output_policy, TerminalEchoBudget::new())
    }

    pub(super) fn with_terminal_echo_budget(
        output_policy: ExecuteOutputPolicy,
        terminal_echo_budget: TerminalEchoBudget,
    ) -> Self {
        match output_policy {
            ExecuteOutputPolicy::Ordinary => Self::Ordinary,
            ExecuteOutputPolicy::SavedWorkflow => Self::SavedWorkflow(Arc::new(Mutex::new(
                SavedAdmissionState::Open(OpenSavedAdmission {
                    bounds: WorkflowOutputBounds::default(),
                    terminal_echo: TerminalEchoState::Empty,
                    terminal_echo_budget,
                }),
            ))),
        }
    }

    pub(super) fn admit_response(
        &self,
        response: &mut RuntimeResponse,
        delivery: ResponseDelivery,
    ) -> AdmissionOutcome {
        if matches!(self, Self::Ordinary) {
            return AdmissionOutcome::Admitted;
        }

        let (cell_id, redact_error) = match response {
            RuntimeResponse::Yielded { cell_id, .. }
            | RuntimeResponse::Terminated { cell_id, .. } => (cell_id.clone(), false),
            RuntimeResponse::Result {
                cell_id,
                error_text,
                ..
            } => (cell_id.clone(), error_text.is_some()),
        };
        let outcome = self.admit_saved_response(response, delivery);
        match outcome {
            AdmissionOutcome::Admitted => {
                if redact_error && let RuntimeResponse::Result { error_text, .. } = response {
                    *error_text = Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string());
                }
            }
            AdmissionOutcome::ExecutionFailed | AdmissionOutcome::Rejected => {
                *response = RuntimeResponse::Result {
                    cell_id,
                    content_items: Vec::new(),
                    error_text: Some(visible_saved_error(outcome).to_string()),
                };
            }
        }
        outcome
    }

    pub(super) fn admit_error(&self, error: String) -> (String, AdmissionOutcome) {
        if matches!(self, Self::Ordinary) {
            return (error, AdmissionOutcome::Admitted);
        }
        let outcome = self.admit_saved(&[], Some(&error));
        (visible_saved_error(outcome).to_string(), outcome)
    }

    pub(super) fn admit_fatal_error(&self, error: String) -> (String, AdmissionOutcome) {
        let Self::SavedWorkflow(shared) = self else {
            return (error, AdmissionOutcome::Admitted);
        };
        let mut admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = match &mut *admission {
            SavedAdmissionState::ExecutionFailed => AdmissionOutcome::ExecutionFailed,
            SavedAdmissionState::Rejected => AdmissionOutcome::Rejected,
            SavedAdmissionState::Open(open) => {
                if open.bounds.admit_response(&[], Some(&error)).is_ok() {
                    AdmissionOutcome::ExecutionFailed
                } else {
                    AdmissionOutcome::Rejected
                }
            }
        };
        match outcome {
            AdmissionOutcome::Admitted => {}
            AdmissionOutcome::ExecutionFailed => {
                *admission = SavedAdmissionState::ExecutionFailed;
            }
            AdmissionOutcome::Rejected => {
                *admission = SavedAdmissionState::Rejected;
            }
        }
        (visible_saved_error(outcome).to_string(), outcome)
    }

    pub(super) fn visible_connection_failure(&self, reason: String) -> String {
        let Self::SavedWorkflow(shared) = self else {
            return reason;
        };
        let admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = match &*admission {
            SavedAdmissionState::Open(_) | SavedAdmissionState::ExecutionFailed => {
                AdmissionOutcome::ExecutionFailed
            }
            SavedAdmissionState::Rejected => AdmissionOutcome::Rejected,
        };
        visible_saved_error(outcome).to_string()
    }

    fn admit_saved(
        &self,
        items: &[FunctionCallOutputContentItem],
        error_text: Option<&str>,
    ) -> AdmissionOutcome {
        let Self::SavedWorkflow(shared) = self else {
            return AdmissionOutcome::Admitted;
        };
        let mut admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *admission {
            SavedAdmissionState::Open(open) => {
                if open.bounds.admit_response(items, error_text).is_ok() {
                    AdmissionOutcome::Admitted
                } else {
                    *admission = SavedAdmissionState::Rejected;
                    AdmissionOutcome::Rejected
                }
            }
            SavedAdmissionState::ExecutionFailed => AdmissionOutcome::ExecutionFailed,
            SavedAdmissionState::Rejected => AdmissionOutcome::Rejected,
        }
    }

    fn admit_saved_response(
        &self,
        response: &RuntimeResponse,
        delivery: ResponseDelivery,
    ) -> AdmissionOutcome {
        let Self::SavedWorkflow(shared) = self else {
            return AdmissionOutcome::Admitted;
        };
        let mut admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = match &mut *admission {
            SavedAdmissionState::Open(open) => open.admit_response(response, delivery),
            SavedAdmissionState::ExecutionFailed => AdmissionOutcome::ExecutionFailed,
            SavedAdmissionState::Rejected => AdmissionOutcome::Rejected,
        };
        match outcome {
            AdmissionOutcome::Admitted => {}
            AdmissionOutcome::ExecutionFailed => {
                *admission = SavedAdmissionState::ExecutionFailed;
            }
            AdmissionOutcome::Rejected => {
                *admission = SavedAdmissionState::Rejected;
            }
        }
        outcome
    }
}

impl OpenSavedAdmission {
    fn admit_response(
        &mut self,
        response: &RuntimeResponse,
        delivery: ResponseDelivery,
    ) -> AdmissionOutcome {
        if delivery == ResponseDelivery::Uncorrelated
            || matches!(response, RuntimeResponse::Yielded { .. })
                && delivery == ResponseDelivery::Observer
        {
            return self.admit_authored_response(response);
        }
        if matches!(response, RuntimeResponse::Yielded { .. }) {
            return self.fail_terminal_delivery();
        }

        match std::mem::replace(&mut self.terminal_echo, TerminalEchoState::Empty) {
            TerminalEchoState::Empty => {
                let outcome = self.admit_authored_response(response);
                if outcome != AdmissionOutcome::Admitted {
                    return outcome;
                }
                let Some(permit) = Arc::clone(&self.terminal_echo_budget.0)
                    .try_acquire_owned()
                    .ok()
                else {
                    return self.fail_terminal_delivery();
                };
                self.terminal_echo = TerminalEchoState::Unmatched {
                    role: delivery,
                    response: response.clone(),
                    _permit: permit,
                };
                AdmissionOutcome::Admitted
            }
            TerminalEchoState::Unmatched {
                role,
                response: first_response,
                _permit,
            } if role != delivery && &first_response == response => {
                self.terminal_echo = TerminalEchoState::Matched;
                AdmissionOutcome::Admitted
            }
            TerminalEchoState::Unmatched { .. } | TerminalEchoState::Matched => {
                self.fail_terminal_delivery()
            }
        }
    }

    fn admit_authored_response(&mut self, response: &RuntimeResponse) -> AdmissionOutcome {
        let (items, error_text) = match response {
            RuntimeResponse::Yielded { content_items, .. }
            | RuntimeResponse::Terminated { content_items, .. } => (content_items.as_slice(), None),
            RuntimeResponse::Result {
                content_items,
                error_text,
                ..
            } => (content_items.as_slice(), error_text.as_deref()),
        };
        if self.bounds.admit_response(items, error_text).is_ok() {
            AdmissionOutcome::Admitted
        } else {
            AdmissionOutcome::Rejected
        }
    }

    fn fail_terminal_delivery(&mut self) -> AdmissionOutcome {
        self.terminal_echo = TerminalEchoState::Empty;
        if self
            .bounds
            .admit_response(&[], Some(INVALID_TERMINAL_DELIVERY))
            .is_ok()
        {
            AdmissionOutcome::ExecutionFailed
        } else {
            AdmissionOutcome::Rejected
        }
    }
}

impl TerminalEchoBudget {
    pub(super) fn new() -> Self {
        Self(Arc::new(Semaphore::new(MAX_UNMATCHED_TERMINAL_ECHOES)))
    }
}

fn visible_saved_error(outcome: AdmissionOutcome) -> &'static str {
    match outcome {
        AdmissionOutcome::Admitted | AdmissionOutcome::ExecutionFailed => {
            SAVED_WORKFLOW_EXECUTION_FAILED
        }
        AdmissionOutcome::Rejected => SAVED_WORKFLOW_OUTPUT_REJECTED,
    }
}

#[cfg(test)]
#[path = "output_admission_tests.rs"]
mod tests;
