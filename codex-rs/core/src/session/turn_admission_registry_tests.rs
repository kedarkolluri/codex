use std::sync::Arc;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::time::timeout;

use super::TurnAdmissionOutcome;
use super::TurnAdmissionRegistrationError;
use super::TurnAdmissionRegistry;
use crate::session::handlers::shutdown_after_stopping_turn_admissions;

#[tokio::test]
async fn registration_drop_before_enqueue_removes_admission() {
    let registry = Arc::new(TurnAdmissionRegistry::default());
    let (registration, response_rx) = registry
        .register("turn-1".to_string())
        .expect("register admission");

    drop(registration);

    assert!(registry.take("turn-1").is_none());
    assert!(response_rx.await.is_err());
}

#[tokio::test]
async fn committed_registration_remains_owned_by_session_loop() {
    let registry = Arc::new(TurnAdmissionRegistry::default());
    let (mut registration, response_rx) = registry
        .register("turn-1".to_string())
        .expect("register admission");
    registration.commit();
    drop(registration);

    registry
        .take("turn-1")
        .expect("committed admission should remain registered")
        .resolve(TurnAdmissionOutcome::Started);

    assert_eq!(
        response_rx.await.expect("admission response"),
        TurnAdmissionOutcome::Started
    );
}

#[tokio::test]
async fn duplicate_registration_does_not_replace_first_waiter() {
    let registry = Arc::new(TurnAdmissionRegistry::default());
    let (registration, response_rx) = registry
        .register("turn-1".to_string())
        .expect("register first admission");

    let Err(error) = registry.register("turn-1".to_string()) else {
        panic!("duplicate admission should be rejected");
    };
    assert_eq!(
        error,
        TurnAdmissionRegistrationError::Duplicate {
            submission_id: "turn-1".to_string(),
        }
    );
    registry
        .take("turn-1")
        .expect("first admission should remain registered")
        .resolve(TurnAdmissionOutcome::Started);
    assert_eq!(
        response_rx.await.expect("first admission response"),
        TurnAdmissionOutcome::Started
    );
    drop(registration);
}

#[tokio::test]
async fn session_loop_exit_fails_current_and_future_admissions() {
    let registry = Arc::new(TurnAdmissionRegistry::default());
    let loop_guard = registry.loop_guard();
    let (mut registration, response_rx) = registry
        .register("turn-1".to_string())
        .expect("register admission");
    registration.commit();
    drop(registration);

    drop(loop_guard);

    assert_eq!(
        response_rx.await.expect("admission response"),
        TurnAdmissionOutcome::SessionLoopStopped
    );
    assert!(registry.take("turn-1").is_none());
    let Err(error) = registry.register("turn-2".to_string()) else {
        panic!("stopped registry should reject future admissions");
    };
    assert_eq!(error, TurnAdmissionRegistrationError::SessionLoopStopped);
}

#[tokio::test]
async fn explicit_shutdown_stops_admissions_before_awaiting_teardown() {
    let registry = Arc::new(TurnAdmissionRegistry::default());
    let loop_guard = registry.loop_guard();
    let (mut registration, response_rx) = registry
        .register("turn-1".to_string())
        .expect("register admission");
    registration.commit();
    drop(registration);
    let (teardown_started_tx, teardown_started_rx) = tokio::sync::oneshot::channel();
    let (release_teardown_tx, release_teardown_rx) = tokio::sync::oneshot::channel();

    let shutdown_task = tokio::spawn(async move {
        let mut loop_guard = Some(loop_guard);
        shutdown_after_stopping_turn_admissions(&mut loop_guard, async move {
            teardown_started_tx
                .send(())
                .expect("signal teardown started");
            release_teardown_rx.await.expect("release teardown");
            true
        })
        .await
    });
    teardown_started_rx.await.expect("teardown should start");

    assert_eq!(
        timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("admission should resolve before teardown completes")
            .expect("admission response"),
        TurnAdmissionOutcome::SessionLoopStopped
    );
    release_teardown_tx.send(()).expect("release teardown");
    assert!(shutdown_task.await.expect("shutdown task"));
}
