use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireWorkflowCellId;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::super::ConnectionDriver;
use super::super::DriverCommand;
use super::super::DriverLifecycle;
use super::super::RemoteSession;
use super::super::SessionCleanup;
use super::super::output_admission::RemoteOutputAdmission;
use super::CellProvenance;
use super::ResolvedCellId;
use super::WORKFLOW_CELL_ID_REJECTED;
use super::WorkflowCellNamespace;
use crate::NoopCodeModeSessionDelegate;
use crate::remote_session::connection::handshake::NegotiatedCapabilities;

const CURRENT_EPOCH: &str = "0123456789abcdef0123456789abcdef";
const FOREIGN_EPOCH: &str = "fedcba9876543210fedcba9876543210";

fn namespace(epoch: &str) -> WorkflowCellNamespace {
    WorkflowCellNamespace::V1 {
        epoch: epoch.to_string(),
    }
}

fn session(generation: u64) -> RemoteSession {
    RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation,
    }
}

fn workflow_cell_id(epoch: &str, sequence: u64) -> WireCellId {
    WireCellId::from(
        &WireWorkflowCellId::try_new(format!("wf:1:{epoch}:{sequence}")).expect("workflow cell ID"),
    )
}

#[test]
fn negotiated_namespace_preserves_only_current_epoch_ids() {
    let namespace = namespace(CURRENT_EPOCH);
    let session = session(/*generation*/ 9);
    let current = workflow_cell_id(CURRENT_EPOCH, /*sequence*/ 1);
    let public = CellId::new(current.as_str().to_string());

    assert_eq!(
        namespace.public_cell_id(session.generation, &current),
        public
    );
    assert_eq!(
        namespace.remote_cell_id(&session, &public),
        Ok(ResolvedCellId {
            wire_id: current.clone(),
            provenance: CellProvenance::SavedWorkflow,
        })
    );
    assert_eq!(
        namespace.remote_cell_id(
            &session,
            &CellId::new(format!("g{}:{}", session.generation, current.as_str())),
        ),
        Err(WORKFLOW_CELL_ID_REJECTED.to_string())
    );

    let foreign = workflow_cell_id(FOREIGN_EPOCH, /*sequence*/ 2);
    let foreign_public = CellId::new(format!("g{}:{}", session.generation, foreign.as_str()));
    assert_eq!(
        namespace.public_cell_id(session.generation, &foreign),
        foreign_public
    );
    assert_eq!(
        namespace.remote_cell_id(&session, &foreign_public),
        Ok(ResolvedCellId {
            wire_id: foreign,
            provenance: CellProvenance::Ordinary,
        })
    );
}

#[test]
fn capability_off_and_reconnect_keep_existing_generation_rules() {
    let current = workflow_cell_id(CURRENT_EPOCH, /*sequence*/ 1);
    let unavailable = WorkflowCellNamespace::Unavailable;
    let first_session = session(/*generation*/ 1);
    let public = unavailable.public_cell_id(first_session.generation, &current);
    assert_eq!(
        unavailable.remote_cell_id(&first_session, &public),
        Ok(ResolvedCellId {
            wire_id: current.clone(),
            provenance: CellProvenance::Ordinary,
        })
    );

    let restarted_session = session(/*generation*/ 2);
    assert_eq!(
        namespace(FOREIGN_EPOCH).remote_cell_id(&restarted_session, &public),
        Err(format!(
            "cell {public} belongs to a stale code-mode host generation"
        ))
    );
}

#[tokio::test]
async fn retired_workflow_wait_and_terminate_complete_locally() {
    let (_command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(/*max_capacity*/ 3);
    let (mut driver, _execute_claim_tx) = ConnectionDriver::new(
        command_rx,
        event_rx,
        event_tx,
        outgoing_tx,
        NegotiatedCapabilities::default(),
        DriverLifecycle {
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(StdMutex::new(None)),
            cancellation: CancellationToken::new(),
        },
    );
    driver.workflow_cell_ids = namespace(CURRENT_EPOCH);
    let session = session(/*generation*/ 3);
    driver.sessions.insert_ready(
        session.clone(),
        Arc::new(NoopCodeModeSessionDelegate),
        SessionCleanup::new(),
    );
    let wire_id = workflow_cell_id(CURRENT_EPOCH, /*sequence*/ 7);
    let public_id = driver
        .sessions
        .admit_cell(
            &session,
            wire_id.clone(),
            RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow),
            &driver.workflow_cell_ids,
        )
        .unwrap_or_else(|_| panic!("live workflow cell"));
    assert!(driver.close_cell(session.id.clone(), wire_id));

    let missing = || {
        Ok(WaitOutcome::MissingCell(RuntimeResponse::Result {
            cell_id: public_id.clone(),
            content_items: Vec::new(),
            error_text: Some(format!("exec cell {public_id} not found")),
        }))
    };
    let (wait_tx, mut wait_rx) = oneshot::channel();
    assert!(driver.handle_command(DriverCommand::Wait {
        session: session.clone(),
        request: WaitRequest {
            cell_id: public_id.clone(),
            yield_time_ms: 1,
        },
        caller_cancellation: CancellationToken::new(),
        response_tx: wait_tx,
    }));
    assert_eq!(wait_rx.try_recv(), Ok(missing()));

    let (terminate_tx, mut terminate_rx) = oneshot::channel();
    assert!(driver.handle_command(DriverCommand::Terminate {
        session: session.clone(),
        cell_id: public_id.clone(),
        response_tx: terminate_tx,
    }));
    assert_eq!(terminate_rx.try_recv(), Ok(missing()));
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let (ordinary_tx, _ordinary_rx) = oneshot::channel();
    assert!(driver.handle_command(DriverCommand::Wait {
        session,
        request: WaitRequest {
            cell_id: CellId::new("g3:ordinary-missing".to_string()),
            yield_time_ms: 1,
        },
        caller_cancellation: CancellationToken::new(),
        response_tx: ordinary_tx,
    }));
    assert!(outgoing_rx.try_recv().is_ok());
}
