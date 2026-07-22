use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc as std_mpsc;

use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WorkflowOutputBounds;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use super::RuntimeCommand;
use super::RuntimeEvent;
use super::timers;

#[derive(Clone)]
pub(super) enum RuntimeOutputAdmission {
    Ordinary,
    SavedWorkflow(Arc<Mutex<SavedWorkflowOutputAdmission>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AdmissionOutcome {
    Admitted,
    Rejected,
}

#[derive(Default)]
pub(super) struct SavedWorkflowOutputAdmission {
    bounds: WorkflowOutputBounds,
    rejection: Option<&'static str>,
}

impl RuntimeOutputAdmission {
    pub(super) fn new(output_policy: ExecuteOutputPolicy) -> Self {
        match output_policy {
            ExecuteOutputPolicy::Ordinary => Self::Ordinary,
            ExecuteOutputPolicy::SavedWorkflow => Self::SavedWorkflow(Arc::new(Mutex::new(
                SavedWorkflowOutputAdmission::default(),
            ))),
        }
    }

    pub(super) fn admit_items(&self, items: &[FunctionCallOutputContentItem]) -> AdmissionOutcome {
        let Self::SavedWorkflow(shared) = self else {
            return AdmissionOutcome::Admitted;
        };
        let mut admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if admission.rejection.is_some() {
            return AdmissionOutcome::Rejected;
        }
        if admission
            .bounds
            .admit_response(items, /*error_text*/ None)
            .is_err()
        {
            admission
                .rejection
                .get_or_insert(SAVED_WORKFLOW_OUTPUT_REJECTED);
            return AdmissionOutcome::Rejected;
        }
        AdmissionOutcome::Admitted
    }

    pub(super) fn terminal_error(
        &self,
        error_text: Option<String>,
    ) -> (Option<String>, AdmissionOutcome) {
        let Self::SavedWorkflow(shared) = self else {
            return (error_text, AdmissionOutcome::Admitted);
        };
        let mut admission = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(rejection) = admission.rejection {
            return (Some(rejection.to_string()), AdmissionOutcome::Rejected);
        }
        let Some(error_text) = error_text else {
            return (None, AdmissionOutcome::Admitted);
        };
        if admission
            .bounds
            .admit_response(&[], Some(&error_text))
            .is_err()
        {
            let rejection = *admission
                .rejection
                .get_or_insert(SAVED_WORKFLOW_OUTPUT_REJECTED);
            return (Some(rejection.to_string()), AdmissionOutcome::Rejected);
        }
        (
            Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
            AdmissionOutcome::Admitted,
        )
    }
}

pub(crate) struct RuntimeState {
    pub(super) event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pub(super) pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    pub(super) stored_values: HashMap<String, JsonValue>,
    pub(super) stored_value_writes: HashMap<String, JsonValue>,
    pub(super) enabled_tools: Vec<EnabledToolMetadata>,
    pub(super) next_tool_call_id: u64,
    pub(super) next_timeout_id: u64,
    pub(super) tool_call_id: String,
    pub(super) runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    pub(super) exit_requested: bool,
    pub(super) output_policy: ExecuteOutputPolicy,
    pub(super) output_admission: RuntimeOutputAdmission,
}
