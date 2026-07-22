use super::*;
use pretty_assertions::assert_eq;

fn insert_terminate(
    driver: &mut ConnectionDriver,
    request_id: RequestId,
    output_admission: RemoteOutputAdmission,
) -> oneshot::Receiver<Result<WaitOutcome, String>> {
    let (response_tx, response_rx) = oneshot::channel();
    let event_tx = driver.event_tx.clone();
    driver.requests.insert_pending(
        request_id,
        PendingRequest::Terminate {
            public_id: CellId::new("cell".to_string()),
            cell_id: WireCellId::try_new("cell").expect("cell ID"),
            output_admission,
            response_tx,
        },
        &event_tx,
    );
    response_rx
}

#[tokio::test]
async fn saved_terminate_invalid_response_and_connection_failure_are_redacted() {
    let (mut driver, alive) = direct_driver();
    ready_session(&mut driver);
    let response_rx = insert_terminate(&mut driver, RequestId::new(/*value*/ 1), saved_admission());
    driver.fail("private peer failure".to_string());
    assert_eq!(
        response_rx.await.expect("connection failure"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));

    let (mut driver, alive) = direct_driver();
    let session = ready_session(&mut driver);
    let request_id = RequestId::new(/*value*/ 1);
    let response_rx = insert_terminate(&mut driver, request_id, saved_admission());
    assert!(!driver.handle_host_message(HostToClient::Response {
        id: request_id,
        result: WireResult::Ok {
            value: HostResponse::SessionClosed {
                session_id: session.id,
            },
        },
    }));
    assert_eq!(
        response_rx.await.expect("invalid response"),
        Err(SAVED_WORKFLOW_EXECUTION_FAILED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropped_terminate_caller_still_records_terminal_before_cell_close() {
    let (mut driver, alive, mut outgoing_rx) = direct_driver_with_outgoing();
    let session = ready_session(&mut driver);
    let remote_cell_id = WireCellId::try_new("cell").expect("cell ID");
    let output_admission = saved_admission();
    let public_cell_id = driver
        .sessions
        .admit_cell(&session, remote_cell_id.clone(), output_admission.clone())
        .unwrap_or_else(|_| panic!("live cell"));
    let initial_id = RequestId::new(/*value*/ 9);
    let initial_rx = insert_initial(&mut driver, initial_id, "cell", output_admission);
    let (response_tx, response_rx) = oneshot::channel();
    assert!(driver.handle_command(DriverCommand::Terminate {
        session: session.clone(),
        cell_id: public_cell_id,
        response_tx,
    }));
    outgoing_rx.recv().await.expect("terminate frame");
    drop(response_rx);

    let raw = WireRuntimeResponse::Terminated {
        cell_id: remote_cell_id.clone(),
        content_items: full_wire_items(),
    };
    assert!(driver.handle_host_message(HostToClient::Response {
        id: RequestId::new(/*value*/ 1),
        result: WireResult::Ok {
            value: HostResponse::WaitCompleted {
                outcome: WireWaitOutcome::LiveCell(raw),
            },
        },
    }));
    assert!(driver.handle_host_message(HostToClient::CellClosed {
        session_id: session.id,
        cell_id: remote_cell_id.clone(),
    }));
    drop(initial_rx);
    assert!(!driver.handle_host_message(HostToClient::InitialResponse {
        id: initial_id,
        result: WireResult::Ok {
            value: WireRuntimeResponse::Terminated {
                cell_id: remote_cell_id,
                content_items: Vec::new(),
            },
        },
    }));
    assert!(!alive.load(Ordering::Acquire));
}

#[tokio::test]
async fn abandoned_saved_execute_retains_admission_after_cell_close() {
    let (mut driver, alive, mut outgoing_rx) = direct_driver_with_outgoing();
    let session = ready_session_with_generation(&mut driver, /*generation*/ 2);
    let execute_id = driver.requests.allocate_id().expect("execute request ID");
    let remote_cell_id = WireCellId::try_new("cell").expect("cell ID");
    let execute_rx = insert_execute_for_session(
        &mut driver,
        execute_id,
        session.clone(),
        full_saved_admission(),
    );

    assert!(driver.handle_host_message(HostToClient::Response {
        id: execute_id,
        result: WireResult::Ok {
            value: HostResponse::ExecutionStarted {
                cell_id: remote_cell_id.clone(),
            },
        },
    }));
    let delivered = execute_rx
        .await
        .expect("execute response")
        .expect("delivered execute");
    let public_id = delivered.started.cell_id.clone();

    assert!(driver.handle_host_message(HostToClient::CellClosed {
        session_id: session.id,
        cell_id: remote_cell_id,
    }));
    assert!(driver.cancel_request(execute_id));
    outgoing_rx
        .recv()
        .await
        .expect("execute cancellation frame");
    outgoing_rx
        .recv()
        .await
        .expect("abandoned cell termination frame");
    let terminate_id = RequestId::new(/*value*/ 2);
    let pending = driver
        .requests
        .remove_pending(terminate_id)
        .expect("abandoned terminate request");
    assert!(matches!(
        &pending,
        PendingRequest::Terminate {
            public_id: pending_id,
            ..
        } if pending_id == &public_id
    ));
    let event_tx = driver.event_tx.clone();
    driver
        .requests
        .insert_pending(terminate_id, pending, &event_tx);

    assert!(!driver.handle_host_message(HostToClient::Response {
        id: terminate_id,
        result: WireResult::Err {
            message: "private abandoned termination failure".to_string(),
        },
    }));
    assert_eq!(
        delivered.started.initial_response().await,
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert!(!alive.load(Ordering::Acquire));
    assert_eq!(
        driver.failure.lock().expect("failure lock").as_deref(),
        Some(SAVED_WORKFLOW_OUTPUT_REJECTED)
    );
}
