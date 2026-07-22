use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireExecuteOutputPolicy;
use codex_code_mode_protocol::host::WireExecuteRequest;
use codex_code_mode_protocol::host::WireWorkflowCellId;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

use super::WorkflowCellSequenceGuard;
use super::WorkflowCellSequenceRejected;
use crate::HostState;
use crate::RequestRegistry;
use crate::SeenSessionIds;
use crate::peer::HostPeer;

const FIRST_EPOCH: &str = "0123456789abcdef0123456789abcdef";
const SECOND_EPOCH: &str = "fedcba9876543210fedcba9876543210";

fn identity(epoch: &str, sequence: u64) -> WireWorkflowCellId {
    WireWorkflowCellId::try_new(format!("wf:1:{epoch}:{sequence}")).expect("workflow cell identity")
}

#[test]
fn replay_and_lower_sequences_remain_rejected_after_long_churn() {
    let mut guard = WorkflowCellSequenceGuard::default();
    for sequence in 1..=10_000 {
        assert_eq!(guard.reserve(&identity(FIRST_EPOCH, sequence)), Ok(()));
    }

    assert_eq!(
        guard.reserve(&identity(FIRST_EPOCH, /*sequence*/ 1)),
        Err(WorkflowCellSequenceRejected)
    );
    assert_eq!(
        guard.reserve(&identity(FIRST_EPOCH, /*sequence*/ 9_999)),
        Err(WorkflowCellSequenceRejected)
    );
}

#[test]
fn one_connection_epoch_accepts_gaps_without_allowing_epoch_switches() {
    let mut guard = WorkflowCellSequenceGuard::default();
    assert_eq!(
        guard.reserve(&identity(FIRST_EPOCH, /*sequence*/ 7)),
        Ok(())
    );
    assert_eq!(
        guard.reserve(&identity(FIRST_EPOCH, /*sequence*/ 42)),
        Ok(())
    );
    assert_eq!(
        guard.reserve(&identity(SECOND_EPOCH, /*sequence*/ 43)),
        Err(WorkflowCellSequenceRejected)
    );
    assert_eq!(
        guard.reserve(&identity(FIRST_EPOCH, /*sequence*/ 43)),
        Ok(())
    );

    let mut fresh_connection = WorkflowCellSequenceGuard::default();
    assert_eq!(
        fresh_connection.reserve(&identity(SECOND_EPOCH, /*sequence*/ 1)),
        Ok(())
    );
}

#[test]
fn host_reserves_identity_before_rejecting_request_capacity() {
    let (outgoing_tx, _outgoing_rx) = mpsc::channel(/*max_capacity*/ 1);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let selected_capabilities = CapabilitySet::try_new([
        Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY).expect("saved output capability"),
        Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY).expect("cell identity capability"),
    ])
    .expect("paired capabilities");
    let state = Arc::new(HostState {
        sessions: Mutex::new(HashMap::new()),
        seen_session_ids: Mutex::new(SeenSessionIds::default()),
        requests: Mutex::new(RequestRegistry::default()),
        request_tasks: TaskTracker::new(),
        request_permits: Arc::new(Semaphore::new(/*permits*/ 0)),
        active_cell_permits: Arc::new(Semaphore::new(/*permits*/ 1)),
        workflow_cell_sequence: Mutex::new(WorkflowCellSequenceGuard::default()),
        selected_capabilities,
        closing: AtomicBool::new(false),
        peer,
    });
    let workflow_cell_id = identity(FIRST_EPOCH, /*sequence*/ 1);

    state
        .spawn_request(
            RequestId::new(/*value*/ 1),
            HostRequest::Execute {
                session_id: SessionId::new("session").expect("session ID"),
                request: WireExecuteRequest {
                    tool_call_id: "call-1".to_string(),
                    enabled_tools: Vec::new(),
                    source: "text('unreachable');".to_string(),
                    output_policy: WireExecuteOutputPolicy::SavedWorkflow,
                    workflow_cell_id: Some(workflow_cell_id.clone()),
                    yield_time_ms: None,
                    max_output_tokens: None,
                },
            },
        )
        .expect("spawn capacity-rejected request");

    assert_eq!(
        state
            .workflow_cell_sequence
            .lock()
            .expect("workflow sequence lock")
            .reserve(&workflow_cell_id),
        Err(WorkflowCellSequenceRejected)
    );
}
