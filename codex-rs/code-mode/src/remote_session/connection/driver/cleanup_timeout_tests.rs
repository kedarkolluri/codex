use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireResult;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::ConnectionDriver;
use super::DriverLifecycle;
use super::RemoteSession;
use super::SessionCleanup;
use super::types::DriverCommand;
use super::types::DriverEvent;
use crate::NoopCodeModeSessionDelegate;
use crate::remote_session::connection::handshake::NegotiatedCapabilities;

const AFTER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(11);

struct DirectDriver {
    driver: ConnectionDriver,
    outgoing_rx: mpsc::Receiver<EncodedFrame>,
    alive: Arc<AtomicBool>,
    cancellation: CancellationToken,
}

impl DirectDriver {
    fn new() -> Self {
        let (_command_tx, command_rx) = mpsc::channel(/*max_capacity*/ 1);
        let (event_tx, event_rx) = mpsc::channel(/*max_capacity*/ 4);
        let (outgoing_tx, outgoing_rx) = mpsc::channel(/*max_capacity*/ 4);
        let alive = Arc::new(AtomicBool::new(true));
        let cancellation = CancellationToken::new();
        let (mut driver, _execute_claim_tx) = ConnectionDriver::new(
            command_rx,
            event_rx,
            event_tx,
            outgoing_tx,
            NegotiatedCapabilities::default(),
            DriverLifecycle {
                alive: Arc::clone(&alive),
                failure: Arc::new(Mutex::new(None)),
                cancellation: cancellation.clone(),
            },
        );
        driver.sessions.insert_ready(
            remote_session(),
            Arc::new(NoopCodeModeSessionDelegate),
            SessionCleanup::new(),
        );
        Self {
            driver,
            outgoing_rx,
            alive,
            cancellation,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn terminate_timeout_fails_connection_after_caller_drops() {
    let mut harness = DirectDriver::new();
    let (response_tx, response_rx) = oneshot::channel();
    drop(response_rx);

    assert!(harness.driver.handle_command(DriverCommand::Terminate {
        session: remote_session(),
        cell_id: CellId::new("1".to_string()),
        response_tx,
    }));
    harness.outgoing_rx.recv().await.expect("terminate frame");

    tokio::time::advance(AFTER_CLEANUP_TIMEOUT).await;
    let event = harness
        .driver
        .event_rx
        .recv()
        .await
        .expect("cleanup timeout");
    assert!(matches!(
        &event,
        DriverEvent::RequestTimedOut(id) if *id == RequestId::new(/*value*/ 1)
    ));
    assert!(!harness.driver.handle_event(event));
    assert!(!harness.alive.load(Ordering::Acquire));
    assert!(harness.cancellation.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn open_timeout_fails_connection_and_unblocks_its_waiter() {
    let mut harness = DirectDriver::new();
    let (response_tx, response_rx) = oneshot::channel();
    let session = RemoteSession {
        id: SessionId::new("opening-session").expect("session ID"),
        generation: 1,
    };

    assert!(
        harness
            .driver
            .handle_command(DriverCommand::OpenSession {
                session,
                delegate: Arc::new(NoopCodeModeSessionDelegate),
                cleanup: SessionCleanup::new(),
                caller_cancellation: CancellationToken::new(),
                response_tx,
            })
    );
    harness.outgoing_rx.recv().await.expect("open frame");

    tokio::time::advance(AFTER_CLEANUP_TIMEOUT).await;
    let event = harness
        .driver
        .event_rx
        .recv()
        .await
        .expect("cleanup timeout");
    assert!(!harness.driver.handle_event(event));
    assert_eq!(
        response_rx.await.expect("open response"),
        Err("timed out opening a code-mode host session".to_string())
    );
    assert!(!harness.alive.load(Ordering::Acquire));
    assert!(harness.cancellation.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn shutdown_timeout_fails_its_pending_request() {
    let mut harness = DirectDriver::new();
    let (response_tx, response_rx) = oneshot::channel();

    assert!(
        harness
            .driver
            .handle_command(DriverCommand::ShutdownSession {
                session: remote_session(),
                response_tx,
            })
    );
    harness.outgoing_rx.recv().await.expect("shutdown frame");

    tokio::time::advance(AFTER_CLEANUP_TIMEOUT).await;
    let event = harness
        .driver
        .event_rx
        .recv()
        .await
        .expect("cleanup timeout");
    assert!(!harness.driver.handle_event(event));
    assert_eq!(
        response_rx.await.expect("shutdown response"),
        Err("timed out shutting down a code-mode host session".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn completed_cleanup_cancels_its_timeout() {
    let mut harness = DirectDriver::new();
    let (response_tx, response_rx) = oneshot::channel();
    let session = remote_session();

    assert!(
        harness
            .driver
            .handle_command(DriverCommand::ShutdownSession {
                session: session.clone(),
                response_tx,
            })
    );
    harness.outgoing_rx.recv().await.expect("shutdown frame");
    assert!(harness.driver.handle_host_message(HostToClient::Response {
        id: RequestId::new(/*value*/ 1),
        result: WireResult::Ok {
            value: HostResponse::SessionClosed {
                session_id: session.id,
            },
        },
    }));
    assert_eq!(response_rx.await.expect("shutdown response"), Ok(()));

    tokio::time::advance(AFTER_CLEANUP_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert!(harness.driver.event_rx.try_recv().is_err());
    assert!(harness.alive.load(Ordering::Acquire));
    assert!(!harness.cancellation.is_cancelled());
}

fn remote_session() -> RemoteSession {
    RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation: 1,
    }
}
