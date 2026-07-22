use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_BYTES;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateRequestId;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::InvalidWireCellId;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireContentItem;
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
use super::super::cell_ids::validate_host_cell_ids;
use super::super::output_admission::AdmissionOutcome;
use super::super::output_admission::RemoteOutputAdmission;
use super::super::types::CancellableRequest;
use super::super::types::DeliveredExecute;
use super::super::types::InitialResponse;
use super::super::types::PendingRequest;
use super::ConnectionDriver;

fn direct_driver() -> (ConnectionDriver, Arc<AtomicBool>) {
    let (driver, alive, _outgoing_rx) = direct_driver_with_outgoing();
    (driver, alive)
}

fn direct_driver_with_outgoing() -> (
    ConnectionDriver,
    Arc<AtomicBool>,
    mpsc::Receiver<EncodedFrame>,
) {
    let (_command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 1);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(/*max_capacity*/ 8);
    let alive = Arc::new(AtomicBool::new(true));
    let cancellation = CancellationToken::new();
    let (driver, _execute_claim_tx) = ConnectionDriver::new(
        command_rx,
        event_rx,
        event_tx,
        outgoing_tx,
        DriverLifecycle {
            alive: Arc::clone(&alive),
            failure: Arc::new(StdMutex::new(None)),
            cancellation,
        },
    );
    (driver, alive, outgoing_rx)
}

fn ready_session(driver: &mut ConnectionDriver) -> RemoteSession {
    let session = RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation: 1,
    };
    driver.sessions.insert_ready(
        session.clone(),
        Arc::new(NoopCodeModeSessionDelegate),
        SessionCleanup::new(),
    );
    session
}

fn saved_admission() -> RemoteOutputAdmission {
    RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow)
}

fn full_wire_items() -> Vec<WireContentItem> {
    (0..WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES)
        .map(|_| WireContentItem::InputText {
            text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2),
        })
        .collect()
}

fn yielded_wait_response(request_id: i64, content_items: Vec<WireContentItem>) -> HostToClient {
    HostToClient::Response {
        id: RequestId::new(request_id),
        result: WireResult::Ok {
            value: HostResponse::WaitCompleted {
                outcome: WireWaitOutcome::LiveCell(WireRuntimeResponse::Yielded {
                    cell_id: WireCellId::try_new("cell").expect("cell ID"),
                    content_items,
                }),
            },
        },
    }
}

fn full_saved_admission() -> RemoteOutputAdmission {
    let admission = saved_admission();
    let mut response = RuntimeResponse::Yielded {
        cell_id: CellId::new("full".to_string()),
        content_items: full_wire_items().into_iter().map(Into::into).collect(),
    };
    assert_eq!(
        admission.admit_response(&mut response),
        super::super::output_admission::AdmissionOutcome::Admitted
    );
    admission
}

fn insert_execute(
    driver: &mut ConnectionDriver,
    request_id: RequestId,
    output_admission: RemoteOutputAdmission,
) -> oneshot::Receiver<Result<DeliveredExecute, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    let (initial_response_tx, initial_response_rx) = oneshot::channel();
    let event_tx = driver.event_tx.clone();
    driver.requests.insert_pending(
        request_id,
        PendingRequest::Execute {
            session: RemoteSession {
                id: SessionId::new("session").expect("session ID"),
                generation: 1,
            },
            response_tx,
            initial_response_tx,
            initial_response_rx,
            output_admission,
            cancellation: CancellableRequest::new(CancellationToken::new()),
        },
        &event_tx,
    );
    response_rx
}

fn insert_initial(
    driver: &mut ConnectionDriver,
    request_id: RequestId,
    cell_id: &str,
    output_admission: RemoteOutputAdmission,
) -> oneshot::Receiver<Result<RuntimeResponse, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    driver.requests.insert_initial_response(
        request_id,
        InitialResponse {
            generation: 1,
            cell_id: WireCellId::try_new(cell_id).expect("cell ID"),
            output_admission,
            response_tx,
        },
    );
    response_rx
}

fn insert_wait(
    driver: &mut ConnectionDriver,
    request_id: RequestId,
    output_admission: RemoteOutputAdmission,
) -> oneshot::Receiver<Result<WaitOutcome, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    let event_tx = driver.event_tx.clone();
    driver.requests.insert_pending(
        request_id,
        PendingRequest::Wait {
            session: RemoteSession {
                id: SessionId::new("session").expect("session ID"),
                generation: 1,
            },
            cell_id: WireCellId::try_new("cell").expect("cell ID"),
            output_admission,
            cancellation: CancellableRequest::new(CancellationToken::new()),
            response_tx,
        },
        &event_tx,
    );
    response_rx
}

fn queue_wait(
    driver: &mut ConnectionDriver,
    session: RemoteSession,
    cell_id: CellId,
    caller_cancellation: CancellationToken,
) -> oneshot::Receiver<Result<WaitOutcome, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    assert!(driver.handle_command(DriverCommand::Wait {
        session,
        request: WaitRequest {
            cell_id,
            yield_time_ms: 1,
        },
        caller_cancellation,
        response_tx,
    }));
    response_rx
}

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

#[tokio::test]
async fn saved_driver_errors_are_redacted_before_delivery_and_failure() {
    let (mut driver, alive) = direct_driver();
    let raw_execute_rx =
        insert_execute(&mut driver, RequestId::new(/*value*/ 1), saved_admission());
    let raw_rx = insert_initial(
        &mut driver,
        RequestId::new(/*value*/ 2),
        "raw",
        saved_admission(),
    );
    let pending_execute_rx =
        insert_execute(&mut driver, RequestId::new(/*value*/ 4), saved_admission());
    let pending_initial_rx = insert_initial(
        &mut driver,
        RequestId::new(/*value*/ 5),
        "pending",
        saved_admission(),
    );
    assert!(driver.handle_host_message(HostToClient::Response {
        id: RequestId::new(/*value*/ 1),
        result: WireResult::Err {
            message: "private execute failure".to_string(),
        },
    }));
    let Err(raw_execute_error) = raw_execute_rx.await.expect("raw execute response") else {
        panic!("saved execute error should not start a cell");
    };
    assert_eq!(raw_execute_error, SAVED_WORKFLOW_EXECUTION_FAILED);
    assert!(driver.handle_host_message(HostToClient::InitialResponse {
        id: RequestId::new(/*value*/ 2),
        result: WireResult::Err {
            message: "private initial failure".to_string(),
        },
    }));
    assert_eq!(
        raw_rx.await.expect("raw initial response"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(alive.load(Ordering::Acquire));
    driver.fail("private peer failure".to_string());
    let Err(pending_execute_error) = pending_execute_rx.await.expect("pending execute failure")
    else {
        panic!("connection failure should not start a pending cell");
    };
    assert_eq!(pending_execute_error, SAVED_WORKFLOW_EXECUTION_FAILED);
    assert_eq!(
        pending_initial_rx.await.expect("pending initial failure"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));

    let (mut driver, alive) = direct_driver();
    let invalid_execute_rx =
        insert_execute(&mut driver, RequestId::new(/*value*/ 3), saved_admission());
    let pending_execute_rx = insert_execute(
        &mut driver,
        RequestId::new(/*value*/ 4),
        full_saved_admission(),
    );
    let pending_initial_rx = insert_initial(
        &mut driver,
        RequestId::new(/*value*/ 5),
        "pending",
        full_saved_admission(),
    );
    assert!(!driver.handle_host_message(HostToClient::Response {
        id: RequestId::new(/*value*/ 3),
        result: WireResult::Ok {
            value: HostResponse::SessionClosed {
                session_id: SessionId::new("session").expect("session ID"),
            },
        },
    }));
    let Err(invalid_execute_error) = invalid_execute_rx.await.expect("invalid execute response")
    else {
        panic!("invalid execute response should not start a cell");
    };
    assert_eq!(invalid_execute_error, SAVED_WORKFLOW_EXECUTION_FAILED);
    let Err(pending_execute_error) = pending_execute_rx.await.expect("pending execute failure")
    else {
        panic!("connection failure should not start a pending cell");
    };
    assert_eq!(pending_execute_error, SAVED_WORKFLOW_EXECUTION_FAILED);
    assert_eq!(
        pending_initial_rx.await.expect("pending initial failure"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));
    assert_eq!(
        driver.failure.lock().expect("failure lock").as_deref(),
        Some(SAVED_WORKFLOW_EXECUTION_FAILED)
    );
}

#[tokio::test]
async fn rejected_saved_initial_response_is_delivered_before_connection_failure() {
    let (mut driver, alive) = direct_driver();
    let request_id = RequestId::new(/*value*/ 1);
    let cell_id = WireCellId::try_new("cell").expect("cell ID");
    let response_rx = insert_initial(&mut driver, request_id, "cell", saved_admission());
    assert!(!driver.handle_host_message(HostToClient::InitialResponse {
        id: request_id,
        result: WireResult::Ok {
            value: WireRuntimeResponse::Yielded {
                cell_id,
                content_items: vec![WireContentItem::InputText {
                    text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES),
                }],
            },
        },
    }));
    assert_eq!(
        response_rx.await.expect("initial response"),
        Ok(RuntimeResponse::Result {
            cell_id: CellId::new("cell".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        })
    );
    assert!(!alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn execution_failed_saved_initial_response_is_delivered_before_connection_failure() {
    let (mut driver, alive) = direct_driver();
    let request_id = RequestId::new(/*value*/ 1);
    let admission = saved_admission();
    let sibling = admission.clone();
    assert_eq!(
        admission.admit_fatal_error("private fatal detail".to_string()),
        (
            SAVED_WORKFLOW_EXECUTION_FAILED.to_string(),
            AdmissionOutcome::ExecutionFailed
        )
    );
    let response_rx = insert_initial(&mut driver, request_id, "cell", sibling);

    assert!(!driver.handle_host_message(HostToClient::InitialResponse {
        id: request_id,
        result: WireResult::Ok {
            value: WireRuntimeResponse::Yielded {
                cell_id: WireCellId::try_new("cell").expect("cell ID"),
                content_items: vec![WireContentItem::InputText {
                    text: "must not escape".to_string(),
                }],
            },
        },
    }));
    assert_eq!(
        response_rx.await.expect("initial response"),
        Ok(RuntimeResponse::Result {
            cell_id: CellId::new("cell".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        })
    );
    assert!(!alive.load(Ordering::Acquire));
    assert_eq!(
        driver.failure.lock().expect("failure lock").as_deref(),
        Some(SAVED_WORKFLOW_EXECUTION_FAILED)
    );
}

#[tokio::test]
async fn wait_errors_and_invalid_responses_use_the_selected_output_policy() {
    let cases = [
        (
            saved_admission(),
            "private saved wait failure",
            SAVED_WORKFLOW_EXECUTION_FAILED,
            true,
        ),
        (
            RemoteOutputAdmission::new(ExecuteOutputPolicy::Ordinary),
            "ordinary wait failure",
            "ordinary wait failure",
            true,
        ),
        (
            full_saved_admission(),
            "",
            SAVED_WORKFLOW_OUTPUT_REJECTED,
            false,
        ),
    ];
    for (output_admission, message, expected, keep_running) in cases {
        let (mut driver, alive) = direct_driver();
        let request_id = RequestId::new(/*value*/ 1);
        let response_rx = insert_wait(&mut driver, request_id, output_admission);
        assert_eq!(
            driver.handle_host_message(HostToClient::Response {
                id: request_id,
                result: WireResult::Err {
                    message: message.to_string(),
                },
            }),
            keep_running
        );
        assert_eq!(
            response_rx.await.expect("wait response"),
            Err(expected.to_string())
        );
        assert_eq!(alive.load(Ordering::Acquire), keep_running);
    }

    let (mut driver, alive) = direct_driver();
    let request_id = RequestId::new(/*value*/ 1);
    let response_rx = insert_wait(&mut driver, request_id, saved_admission());
    assert!(!driver.handle_host_message(HostToClient::Response {
        id: request_id,
        result: WireResult::Ok {
            value: HostResponse::SessionClosed {
                session_id: SessionId::new("session").expect("session ID"),
            },
        },
    }));
    assert_eq!(
        response_rx.await.expect("invalid saved wait response"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn saved_initial_and_wait_share_one_remote_output_ledger() {
    let (mut driver, alive, mut outgoing_rx) = direct_driver_with_outgoing();
    let session = ready_session(&mut driver);
    let cell_id = WireCellId::try_new("cell").expect("cell ID");
    let execute_id = driver.requests.allocate_id().expect("execute request ID");
    let execute_rx = insert_execute(&mut driver, execute_id, saved_admission());
    assert!(driver.handle_host_message(HostToClient::Response {
        id: execute_id,
        result: WireResult::Ok {
            value: HostResponse::ExecutionStarted {
                cell_id: cell_id.clone(),
            },
        },
    }));
    let started = execute_rx
        .await
        .expect("execute response")
        .expect("started cell")
        .started;
    let public_id = started.cell_id.clone();

    let initial = WireRuntimeResponse::Yielded {
        cell_id: cell_id.clone(),
        content_items: full_wire_items(),
    };
    let expected_initial: RuntimeResponse = initial.clone().into();
    assert!(driver.handle_host_message(HostToClient::InitialResponse {
        id: execute_id,
        result: WireResult::Ok { value: initial },
    }));
    assert_eq!(started.initial_response().await, Ok(expected_initial));

    let wait_rx = queue_wait(&mut driver, session, public_id, CancellationToken::new());
    outgoing_rx.recv().await.expect("wait frame");
    assert!(!driver.handle_host_message(yielded_wait_response(
        /*request_id*/ 2,
        vec![WireContentItem::InputText {
            text: String::new(),
        }],
    )));
    assert_eq!(
        wait_rx.await.expect("wait response"),
        Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: CellId::new("cell".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }))
    );
    assert!(!alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn saved_deferred_wait_keeps_remote_output_ledger() {
    let (mut driver, alive, mut outgoing_rx) = direct_driver_with_outgoing();
    let session = ready_session(&mut driver);
    let cell_id = WireCellId::try_new("cell").expect("cell ID");
    let execute_id = driver.requests.allocate_id().expect("execute request ID");
    let execute_rx = insert_execute(&mut driver, execute_id, full_saved_admission());
    assert!(driver.handle_host_message(HostToClient::Response {
        id: execute_id,
        result: WireResult::Ok {
            value: HostResponse::ExecutionStarted { cell_id },
        },
    }));
    let started = execute_rx
        .await
        .expect("execute response")
        .expect("started cell")
        .started;

    let first_cancellation = CancellationToken::new();
    first_cancellation.cancel();
    let wait_rx = queue_wait(
        &mut driver,
        session.clone(),
        started.cell_id.clone(),
        first_cancellation,
    );
    outgoing_rx.recv().await.expect("first wait frame");

    let second_cancellation = CancellationToken::new();
    let second_rx = queue_wait(
        &mut driver,
        session.clone(),
        started.cell_id.clone(),
        second_cancellation.clone(),
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    assert!(driver.handle_host_message(yielded_wait_response(/*request_id*/ 2, Vec::new(),)));
    assert!(driver.flush_deferred_waits());
    outgoing_rx.recv().await.expect("deferred wait frame");
    assert_eq!(
        wait_rx.await.expect("first wait response"),
        Ok(WaitOutcome::LiveCell(RuntimeResponse::Yielded {
            cell_id: CellId::new("cell".to_string()),
            content_items: Vec::new(),
        }))
    );

    let pending_rx = queue_wait(
        &mut driver,
        session.clone(),
        started.cell_id.clone(),
        CancellationToken::new(),
    );
    outgoing_rx.recv().await.expect("parallel wait frame");
    second_cancellation.cancel();
    let deferred_rx = queue_wait(
        &mut driver,
        session,
        started.cell_id.clone(),
        CancellationToken::new(),
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(!driver.handle_host_message(yielded_wait_response(
        /*request_id*/ 3,
        vec![WireContentItem::InputText {
            text: String::new(),
        }],
    )));
    assert_eq!(
        second_rx.await.expect("second wait response"),
        Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: CellId::new("cell".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }))
    );
    for response_rx in [pending_rx, deferred_rx] {
        assert_eq!(
            response_rx.await.expect("wait failure"),
            Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
        );
    }
    assert_eq!(
        started.initial_response().await,
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));
}
