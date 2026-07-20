use std::sync::Arc;

use pretty_assertions::assert_eq;

use super::TurnAdmissionOutcome;
use super::TurnAdmissionRegistry;

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
async fn session_loop_exit_fails_committed_admissions() {
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
        TurnAdmissionOutcome::Failed("session loop stopped before turn admission".to_string())
    );
    assert!(registry.take("turn-1").is_none());
}
