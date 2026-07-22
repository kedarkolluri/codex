use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WorkflowOutputBounds;

#[derive(Clone)]
pub(super) enum RemoteOutputAdmission {
    Ordinary,
    SavedWorkflow(Arc<Mutex<SavedAdmissionState>>),
}

pub(super) enum SavedAdmissionState {
    Open(WorkflowOutputBounds),
    ExecutionFailed,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AdmissionOutcome {
    Admitted,
    ExecutionFailed,
    Rejected,
}

impl RemoteOutputAdmission {
    pub(super) fn new(output_policy: ExecuteOutputPolicy) -> Self {
        match output_policy {
            ExecuteOutputPolicy::Ordinary => Self::Ordinary,
            ExecuteOutputPolicy::SavedWorkflow => Self::SavedWorkflow(Arc::new(Mutex::new(
                SavedAdmissionState::Open(WorkflowOutputBounds::default()),
            ))),
        }
    }

    pub(super) fn admit_response(&self, response: &mut RuntimeResponse) -> AdmissionOutcome {
        if matches!(self, Self::Ordinary) {
            return AdmissionOutcome::Admitted;
        }

        let (cell_id, items, error_text) = match response {
            RuntimeResponse::Yielded {
                cell_id,
                content_items,
            }
            | RuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => (cell_id.clone(), content_items.as_slice(), None),
            RuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
            } => (
                cell_id.clone(),
                content_items.as_slice(),
                error_text.as_deref(),
            ),
        };
        let redact_error = error_text.is_some();
        let outcome = self.admit_saved(items, error_text);
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
            SavedAdmissionState::Open(bounds) => {
                if bounds.admit_response(&[], Some(&error)).is_ok() {
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
            SavedAdmissionState::Open(bounds) => {
                if bounds.admit_response(items, error_text).is_ok() {
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
