use super::*;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_BYTES;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::host::WireContentItem;
use pretty_assertions::assert_eq;

use super::super::output_admission::RemoteOutputAdmission;

fn saved_harness() -> (DriverHarness, RemoteSession, Arc<RecordingDelegate>) {
    let session = remote_session();
    let delegate = Arc::new(RecordingDelegate::default());
    let configured_session = session.clone();
    let configured_delegate = Arc::clone(&delegate);
    let harness = DriverHarness::start_configured(move |driver| {
        driver.sessions.insert_ready(
            configured_session.clone(),
            configured_delegate,
            SessionCleanup::new(),
        );
        let admission = RemoteOutputAdmission::with_terminal_echo_budget(
            ExecuteOutputPolicy::SavedWorkflow,
            driver.terminal_echo_budget.clone(),
        );
        driver
            .sessions
            .admit_cell(
                &configured_session,
                CellId::new("1".to_string()).into(),
                admission,
            )
            .unwrap_or_else(|_| panic!("live cell"));
    });
    (harness, session, delegate)
}

async fn queue_wait(
    harness: &mut DriverHarness,
    session: &RemoteSession,
    caller_cancellation: CancellationToken,
) -> oneshot::Receiver<Result<WaitOutcome, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Wait {
            session: session.clone(),
            request: WaitRequest {
                cell_id: CellId::new("1".to_string()),
                yield_time_ms: 1,
            },
            caller_cancellation,
            response_tx,
        })
        .await
        .expect("wait command");
    harness.outgoing_rx.recv().await.expect("wait frame");
    response_rx
}

async fn queue_terminate(
    harness: &mut DriverHarness,
    session: &RemoteSession,
) -> oneshot::Receiver<Result<WaitOutcome, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    harness
        .command_tx
        .send(DriverCommand::Terminate {
            session: session.clone(),
            cell_id: CellId::new("1".to_string()),
            response_tx,
        })
        .await
        .expect("terminate command");
    harness.outgoing_rx.recv().await.expect("terminate frame");
    response_rx
}

fn private_terminal_result() -> WireRuntimeResponse {
    WireRuntimeResponse::Result {
        cell_id: CellId::new("1".to_string()).into(),
        content_items: Vec::new(),
        error_text: Some("private terminal detail".to_string()),
    }
}

fn full_terminal_response() -> WireRuntimeResponse {
    let content_items = (0..WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES)
        .map(|_| WireContentItem::InputText {
            text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2),
        })
        .collect();
    WireRuntimeResponse::Terminated {
        cell_id: CellId::new("1".to_string()).into(),
        content_items,
    }
}

fn empty_terminal_response() -> WireRuntimeResponse {
    WireRuntimeResponse::Terminated {
        cell_id: CellId::new("1".to_string()).into(),
        content_items: Vec::new(),
    }
}

async fn send_live_response(
    harness: &DriverHarness,
    request_id: i64,
    response: WireRuntimeResponse,
) {
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(request_id),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::LiveCell(response),
                },
            },
        }))
        .await
        .expect("live-cell response");
}

async fn send_missing_response(
    harness: &DriverHarness,
    request_id: i64,
    response: WireRuntimeResponse,
) {
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(request_id),
            result: WireResult::Ok {
                value: HostResponse::WaitCompleted {
                    outcome: WireWaitOutcome::MissingCell(response),
                },
            },
        }))
        .await
        .expect("missing-cell response");
}

async fn close_cell(harness: &DriverHarness, session: &RemoteSession) {
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::CellClosed {
            session_id: session.id.clone(),
            cell_id: CellId::new("1".to_string()).into(),
        }))
        .await
        .expect("cell close");
}

#[tokio::test]
async fn saved_terminate_transport_error_does_not_claim_terminal_role() {
    let (mut harness, session, _delegate) = saved_harness();
    let error_rx = queue_terminate(&mut harness, &session).await;
    harness
        .event_tx
        .send(DriverEvent::HostMessage(HostToClient::Response {
            id: RequestId::new(/*value*/ 1),
            result: WireResult::Err {
                message: "private terminate transport failure".to_string(),
            },
        }))
        .await
        .expect("terminate error");
    assert_eq!(
        error_rx.await.expect("terminate response"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );

    let wait_rx = queue_wait(&mut harness, &session, CancellationToken::new()).await;
    let terminate_rx = queue_terminate(&mut harness, &session).await;
    let raw = private_terminal_result();
    send_live_response(&harness, /*request_id*/ 2, raw.clone()).await;
    send_live_response(&harness, /*request_id*/ 3, raw).await;
    let expected = Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
        cell_id: CellId::new("1".to_string()),
        content_items: Vec::new(),
        error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
    }));
    for response_rx in [wait_rx, terminate_rx] {
        assert_eq!(response_rx.await.expect("terminal response"), expected);
    }
    assert!(harness.alive.load(Ordering::Acquire));
}

#[derive(Clone, Copy)]
enum CellCloseOrder {
    BeforeResponses,
    BetweenTerminateAndObserver,
}

async fn assert_saved_echo_survives_cell_close(order: CellCloseOrder) {
    let (mut harness, session, delegate) = saved_harness();
    let wait_rx = queue_wait(&mut harness, &session, CancellationToken::new()).await;
    let terminate_rx = queue_terminate(&mut harness, &session).await;
    let raw = full_terminal_response();

    match order {
        CellCloseOrder::BeforeResponses => {
            drop(wait_rx);
            close_cell(&harness, &session).await;
            send_live_response(&harness, /*request_id*/ 1, raw.clone()).await;
            send_live_response(&harness, /*request_id*/ 2, raw).await;
            assert!(terminate_rx.await.expect("terminate response").is_ok());
        }
        CellCloseOrder::BetweenTerminateAndObserver => {
            drop(terminate_rx);
            send_live_response(&harness, /*request_id*/ 2, raw.clone()).await;
            close_cell(&harness, &session).await;
            send_live_response(&harness, /*request_id*/ 1, raw).await;
            assert!(wait_rx.await.expect("observer response").is_ok());
        }
    }

    assert!(harness.alive.load(Ordering::Acquire));
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("1".to_string())]
    );
}

#[tokio::test]
async fn saved_echo_survives_cell_close_before_and_between_responses() {
    for order in [
        CellCloseOrder::BeforeResponses,
        CellCloseOrder::BetweenTerminateAndObserver,
    ] {
        assert_saved_echo_survives_cell_close(order).await;
    }
}

#[tokio::test]
async fn cancelled_saved_observer_still_admits_the_terminal_echo() {
    let (mut harness, session, _delegate) = saved_harness();
    let cancellation = CancellationToken::new();
    let wait_rx = queue_wait(&mut harness, &session, cancellation.clone()).await;
    drop(wait_rx);
    let terminate_rx = queue_terminate(&mut harness, &session).await;
    cancellation.cancel();
    harness
        .outgoing_rx
        .recv()
        .await
        .expect("wait cancellation frame");

    let raw = full_terminal_response();
    send_live_response(&harness, /*request_id*/ 1, raw).await;
    send_live_response(&harness, /*request_id*/ 2, empty_terminal_response()).await;
    assert_eq!(
        terminate_rx.await.expect("terminate response"),
        Ok(WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: CellId::new("1".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }))
    );
    assert!(!harness.alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn saved_missing_cell_output_does_not_clear_the_pending_terminal() {
    let (mut harness, session, _delegate) = saved_harness();
    let observer_rx = queue_wait(&mut harness, &session, CancellationToken::new()).await;
    let terminate_rx = queue_terminate(&mut harness, &session).await;
    let raw = private_terminal_result();
    send_live_response(&harness, /*request_id*/ 1, raw).await;
    assert!(observer_rx.await.expect("observer response").is_ok());

    let missing_rx = queue_wait(&mut harness, &session, CancellationToken::new()).await;
    send_missing_response(
        &harness,
        /*request_id*/ 3,
        WireRuntimeResponse::Terminated {
            cell_id: CellId::new("1".to_string()).into(),
            content_items: vec![WireContentItem::InputText {
                text: "unrelated missing-cell output".to_string(),
            }],
        },
    )
    .await;
    assert!(missing_rx.await.expect("missing-cell response").is_ok());

    send_live_response(&harness, /*request_id*/ 2, empty_terminal_response()).await;
    assert!(terminate_rx.await.expect("terminate response").is_ok());
    assert!(!harness.alive.load(Ordering::Acquire));
}
