use std::sync::Arc;

use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

use super::AttemptFinish;
use super::WorkflowAgentControlAction;
use super::WorkflowAgentControlDisposition;
use super::WorkflowAgentControlRegistry;

const RUN_ID: &str = "0198c0de-0000-7000-8000-000000000064";

#[tokio::test]
async fn skip_duplicates_join_one_cleanup_barrier() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let cancellation = CancellationToken::new();
    let registration = registry
        .register_attempt(RUN_ID, 7, 0, cancellation.clone())
        .expect("register attempt");
    registration.activate();

    let first = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 7, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });
    let duplicate = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 7, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });

    cancellation.cancelled().await;
    assert!(!first.is_finished());
    assert!(!duplicate.is_finished());
    assert_eq!(
        registration.finish_after_cleanup(false),
        AttemptFinish::UserSkip
    );
    assert_eq!(
        first.await.expect("first join"),
        WorkflowAgentControlDisposition::Skipped
    );
    assert_eq!(
        duplicate.await.expect("duplicate join"),
        WorkflowAgentControlDisposition::Skipped
    );
}

#[tokio::test]
async fn retry_reports_next_attempt_only_after_cleanup() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let registration = registry
        .register_attempt(RUN_ID, 9, 3, CancellationToken::new())
        .expect("register attempt");
    registration.activate();
    let control = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 9, 3, WorkflowAgentControlAction::Retry)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!control.is_finished());
    assert_eq!(
        registration.finish_after_cleanup(false),
        AttemptFinish::UserRetry { next_attempt: 4 }
    );
    assert_eq!(
        control.await.expect("retry join"),
        WorkflowAgentControlDisposition::RetryScheduled { attempt: 4 }
    );
}

#[tokio::test]
async fn sixth_attempt_retry_fails_without_a_seventh_generation() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let registration = registry
        .register_attempt(RUN_ID, 3, 5, CancellationToken::new())
        .expect("register attempt");
    registration.activate();
    let control = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 3, 5, WorkflowAgentControlAction::Retry)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        registration.finish_after_cleanup(false),
        AttemptFinish::RetryLimitReached
    );
    assert_eq!(
        control.await.expect("retry cap join"),
        WorkflowAgentControlDisposition::RetryLimitReached
    );
    assert!(
        registry
            .register_attempt(RUN_ID, 3, 6, CancellationToken::new())
            .is_none()
    );
}

#[tokio::test]
async fn malformed_stale_conflicting_and_run_cancelled_controls_fail_closed() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    assert_eq!(
        registry
            .control("not-a-run", 1, 0, WorkflowAgentControlAction::Skip)
            .await,
        WorkflowAgentControlDisposition::Unavailable
    );
    assert_eq!(
        registry
            .control(
                &RUN_ID.to_uppercase(),
                1,
                0,
                WorkflowAgentControlAction::Skip,
            )
            .await,
        WorkflowAgentControlDisposition::Unavailable
    );

    let registration = registry
        .register_attempt(RUN_ID, 1, 0, CancellationToken::new())
        .expect("register attempt");
    registration.activate();
    let retry = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 1, 0, WorkflowAgentControlAction::Retry)
                .await
        }
    });
    tokio::task::yield_now().await;
    let conflicting_skip = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 1, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!conflicting_skip.is_finished());
    registration.claim_run_cancellation();
    assert_eq!(
        registration.finish_after_cleanup(true),
        AttemptFinish::RunCancellation
    );
    assert_eq!(
        retry.await.expect("retry waiter"),
        WorkflowAgentControlDisposition::Unavailable
    );
    assert_eq!(
        conflicting_skip.await.expect("conflicting skip waiter"),
        WorkflowAgentControlDisposition::Unavailable
    );
    assert_eq!(
        registry
            .control(RUN_ID, 1, 0, WorkflowAgentControlAction::Retry)
            .await,
        WorkflowAgentControlDisposition::Unavailable
    );
}

#[tokio::test]
async fn skip_winner_makes_conflicting_retry_join_then_fail_closed() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let registration = registry
        .register_attempt(RUN_ID, 11, 0, CancellationToken::new())
        .expect("register attempt");
    registration.activate();
    let skip = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 11, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });
    tokio::task::yield_now().await;
    let retry = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 11, 0, WorkflowAgentControlAction::Retry)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!skip.is_finished());
    assert!(!retry.is_finished());
    assert_eq!(
        registration.finish_after_cleanup(false),
        AttemptFinish::UserSkip
    );
    assert_eq!(
        skip.await.expect("skip winner"),
        WorkflowAgentControlDisposition::Skipped
    );
    assert_eq!(
        retry.await.expect("retry loser"),
        WorkflowAgentControlDisposition::Unavailable
    );
}

#[tokio::test]
async fn pre_bind_attempt_is_not_selectable_until_activated() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let cancellation = CancellationToken::new();
    let registration = registry
        .register_attempt(RUN_ID, 13, 0, cancellation.clone())
        .expect("register attempt");

    assert_eq!(
        registry
            .control(RUN_ID, 13, 0, WorkflowAgentControlAction::Skip)
            .await,
        WorkflowAgentControlDisposition::Unavailable
    );
    assert!(!cancellation.is_cancelled());

    registration.activate();
    let control = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 13, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });
    cancellation.cancelled().await;
    assert_eq!(
        registration.finish_after_cleanup(false),
        AttemptFinish::UserSkip
    );
    assert_eq!(
        control.await.expect("skip join"),
        WorkflowAgentControlDisposition::Skipped
    );
}

#[tokio::test]
async fn terminal_persistence_failure_fails_control_closed_after_cleanup() {
    let registry = Arc::new(WorkflowAgentControlRegistry::default());
    let registration = registry
        .register_attempt(RUN_ID, 17, 0, CancellationToken::new())
        .expect("register attempt");
    registration.activate();
    let control = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .control(RUN_ID, 17, 0, WorkflowAgentControlAction::Skip)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!control.is_finished());
    assert_eq!(
        registration.decide_after_cleanup(false),
        AttemptFinish::UserSkip
    );
    assert!(!control.is_finished());
    registration.acknowledge_persistence_failure();
    assert_eq!(
        control.await.expect("failed persistence join"),
        WorkflowAgentControlDisposition::Unavailable
    );
}
