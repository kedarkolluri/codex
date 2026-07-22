use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateRequestId;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::InvalidWireCellId;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireNestedToolCall;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireWaitOutcome;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::NoopCodeModeSessionDelegate;

use super::super::DriverCommand;
use super::super::DriverEvent;
use super::super::DriverLifecycle;
use super::super::RemoteSession;
use super::super::SessionCleanup;
use super::ConnectionDriver;
use super::validate_host_cell_ids;

fn invalid_host_cell_id_messages() -> Vec<HostToClient> {
    let invalid_cell_id = || WireCellId::new("invalid\ncell");
    let runtime_response = || WireRuntimeResponse::Result {
        cell_id: invalid_cell_id(),
        content_items: Vec::new(),
        error_text: None,
    };
    let session_id = || SessionId::new("session").expect("session ID");

    vec![
        HostToClient::Response {
            id: RequestId::new(/*value*/ 1),
            result: WireResult::Ok {
                value: HostResponse::ExecutionStarted {
                    cell_id: invalid_cell_id(),
                },
            },
        },
        HostToClient::Response {
            id: RequestId::new(/*value*/ 2),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(runtime_response()),
                },
            },
        },
        HostToClient::InitialResponse {
            id: RequestId::new(/*value*/ 3),
            result: WireResult::Ok {
                value: runtime_response(),
            },
        },
        HostToClient::DelegateRequest {
            id: DelegateRequestId::new(/*value*/ 4),
            session_id: session_id(),
            request: DelegateRequest::InvokeTool {
                invocation: WireNestedToolCall {
                    cell_id: invalid_cell_id(),
                    runtime_tool_call_id: "tool-call".to_string(),
                    tool_name: ToolName::plain("tool").into(),
                    tool_kind: CodeModeToolKind::Function.into(),
                    input: None,
                },
            },
        },
        HostToClient::DelegateRequest {
            id: DelegateRequestId::new(/*value*/ 5),
            session_id: session_id(),
            request: DelegateRequest::Notify {
                call_id: "notify-call".to_string(),
                cell_id: invalid_cell_id(),
                text: "notification".to_string(),
            },
        },
        HostToClient::CellClosed {
            session_id: session_id(),
            cell_id: invalid_cell_id(),
        },
    ]
}

#[test]
fn every_typed_host_cell_id_shape_is_validated() {
    for message in invalid_host_cell_id_messages() {
        assert_eq!(validate_host_cell_ids(&message), Err(InvalidWireCellId));
    }
}

#[tokio::test]
async fn invalid_typed_host_cell_id_fails_connection_and_pending_request() {
    let (command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(/*max_capacity*/ 1);
    let alive = Arc::new(AtomicBool::new(true));
    let failure = Arc::new(StdMutex::new(None));
    let cancellation = CancellationToken::new();
    let (driver, _execute_claim_tx) = ConnectionDriver::new(
        command_rx,
        event_rx,
        event_tx.clone(),
        outgoing_tx,
        DriverLifecycle {
            alive: Arc::clone(&alive),
            failure: Arc::clone(&failure),
            cancellation: cancellation.clone(),
        },
    );
    let driver_task = tokio::spawn(driver.run());
    let session = RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation: 1,
    };
    let (response_tx, response_rx) = oneshot::channel();
    command_tx
        .send(DriverCommand::OpenSession {
            session,
            delegate: Arc::new(NoopCodeModeSessionDelegate),
            cleanup: SessionCleanup::new(),
            caller_cancellation: CancellationToken::new(),
            response_tx,
        })
        .await
        .expect("open command");
    outgoing_rx.recv().await.expect("open frame");

    event_tx
        .send(DriverEvent::HostMessage(
            invalid_host_cell_id_messages().remove(0),
        ))
        .await
        .expect("invalid host response");

    assert_eq!(
        response_rx.await.expect("open reply"),
        Err("invalid code-mode cell ID".to_string())
    );
    driver_task.await.expect("driver task");
    assert!(!alive.load(Ordering::Acquire));
    assert!(cancellation.is_cancelled());
    assert_eq!(
        failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("invalid code-mode cell ID")
    );
}
