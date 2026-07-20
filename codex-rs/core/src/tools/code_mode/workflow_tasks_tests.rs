use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::oneshot;

use super::*;

fn start_error<Start>(rejected: WorkflowTaskRejected<Start>) -> WorkflowTaskStartError {
    rejected.into_parts().0
}

#[tokio::test]
async fn dropping_the_result_handle_does_not_cancel_the_task() {
    let manager = WorkflowTaskManager::default();
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let task_release = Arc::clone(&release);
    let task_completed = Arc::clone(&completed);
    let handle = manager
        .start_recoverable("run-1".to_string(), move |_cancellation| async move {
            task_release.notified().await;
            task_completed.notify_one();
            Ok::<_, ()>(())
        })
        .expect("task starts");

    drop(handle);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("detached task completes");
    manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_waits_for_every_task_cleanup_path_and_rejects_new_work() {
    let manager = WorkflowTaskManager::default();
    let cleaned_a = Arc::new(AtomicBool::new(false));
    let cleaned_b = Arc::new(AtomicBool::new(false));
    for (run_id, cleaned) in [("run-a", &cleaned_a), ("run-b", &cleaned_b)] {
        let task_cleaned = Arc::clone(cleaned);
        manager
            .start_recoverable(run_id.to_string(), move |cancellation| async move {
                cancellation.cancelled().await;
                task_cleaned.store(true, Ordering::Release);
                Ok::<_, ()>(())
            })
            .expect("task starts");
    }

    manager.shutdown().await;

    assert!(cleaned_a.load(Ordering::Acquire));
    assert!(cleaned_b.load(Ordering::Acquire));
    let error = manager
        .start_recoverable("run-c".to_string(), |_cancellation| async {
            Ok::<_, ()>(())
        })
        .expect_err("shutdown is terminal");
    assert_eq!(start_error(error), WorkflowTaskStartError::ShuttingDown);
}

#[tokio::test]
async fn duplicate_active_run_ids_are_rejected() {
    let manager = WorkflowTaskManager::default();
    let _handle = manager
        .start_recoverable("run-1".to_string(), |cancellation| async move {
            cancellation.cancelled().await;
            Ok::<_, ()>(())
        })
        .expect("first task starts");

    let error = manager
        .start_recoverable("run-1".to_string(), |_cancellation| async {
            Ok::<_, ()>(())
        })
        .expect_err("duplicate must fail");

    assert_eq!(start_error(error), WorkflowTaskStartError::AlreadyRunning);
    manager.shutdown().await;
}

#[tokio::test]
async fn cancelling_one_run_does_not_cancel_another() {
    let manager = WorkflowTaskManager::default();
    let cleaned_a = Arc::new(AtomicBool::new(false));
    let cleaned_b = Arc::new(AtomicBool::new(false));
    for (run_id, cleaned) in [
        ("run-a", Arc::clone(&cleaned_a)),
        ("run-b", Arc::clone(&cleaned_b)),
    ] {
        manager
            .start_recoverable(run_id.to_string(), move |cancellation| async move {
                let cause = cancellation.cancelled().await;
                if cause == WorkflowCancellationCause::UserStop {
                    cancellation.resolve_user_stop(/*stopped*/ true);
                }
                cleaned.store(true, Ordering::Release);
                Ok::<_, ()>(())
            })
            .expect("task starts");
    }

    assert_eq!(
        manager.cancel_run("run-a").await,
        WorkflowTaskCancelOutcome::Applied
    );
    assert!(cleaned_a.load(Ordering::Acquire));
    assert!(!cleaned_b.load(Ordering::Acquire));

    manager.shutdown().await;
    assert!(cleaned_b.load(Ordering::Acquire));
}

#[tokio::test]
async fn duplicate_cancellation_requests_share_the_cleanup_wait() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            let cause = cancellation.cancelled().await;
            if cause == WorkflowCancellationCause::UserStop {
                cancellation.resolve_user_stop(/*stopped*/ true);
            }
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let first_manager = manager.clone();
    let first = tokio::spawn(async move { first_manager.cancel_run("run-1").await });
    cancellation_seen.notified().await;

    let second_manager = manager.clone();
    let second = tokio::spawn(async move { second_manager.cancel_run("run-1").await });
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        first.await.expect("first cancellation joins"),
        WorkflowTaskCancelOutcome::Applied
    );
    assert_eq!(
        second.await.expect("second cancellation joins"),
        WorkflowTaskCancelOutcome::AlreadyRequested
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn duplicate_pause_requests_join_one_cleanup_and_preserve_pause_cause() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            assert_eq!(
                cancellation.cancelled().await,
                WorkflowCancellationCause::Pause
            );
            cancellation.resolve_pause(/*paused*/ true);
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let first_manager = manager.clone();
    let first = tokio::spawn(async move { first_manager.pause_run("run-1").await });
    cancellation_seen.notified().await;
    let second_manager = manager.clone();
    let second = tokio::spawn(async move { second_manager.pause_run("run-1").await });
    tokio::task::yield_now().await;
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        first.await.expect("first pause joins cleanup"),
        WorkflowTaskCancelOutcome::Applied
    );
    assert_eq!(
        second.await.expect("second pause joins cleanup"),
        WorkflowTaskCancelOutcome::AlreadyRequested
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn pause_wins_immutable_cause_and_late_stop_reports_not_running() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            assert_eq!(
                cancellation.cancelled().await,
                WorkflowCancellationCause::Pause
            );
            cancellation.resolve_pause(/*paused*/ true);
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let pause_manager = manager.clone();
    let pause = tokio::spawn(async move { pause_manager.pause_run("run-1").await });
    cancellation_seen.notified().await;
    let stop_manager = manager.clone();
    let stop = tokio::spawn(async move { stop_manager.cancel_run("run-1").await });
    tokio::task::yield_now().await;
    release_cleanup.notify_one();

    assert_eq!(
        pause.await.expect("pause joins cleanup"),
        WorkflowTaskCancelOutcome::Applied
    );
    assert_eq!(
        stop.await.expect("late stop joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn stop_wins_immutable_cause_and_late_pause_reports_not_running() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            assert_eq!(
                cancellation.cancelled().await,
                WorkflowCancellationCause::UserStop
            );
            cancellation.resolve_user_stop(/*stopped*/ true);
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let stop_manager = manager.clone();
    let stop = tokio::spawn(async move { stop_manager.cancel_run("run-1").await });
    cancellation_seen.notified().await;
    let pause_manager = manager.clone();
    let pause = tokio::spawn(async move { pause_manager.pause_run("run-1").await });
    tokio::task::yield_now().await;
    release_cleanup.notify_one();

    assert_eq!(
        stop.await.expect("stop joins cleanup"),
        WorkflowTaskCancelOutcome::Applied
    );
    assert_eq!(
        pause.await.expect("late pause joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn natural_completion_wins_before_pause_can_relabel_the_run() {
    let manager = WorkflowTaskManager::default();
    let finish_barrier = Arc::new(tokio::sync::Barrier::new(2));
    let task_finish_barrier = Arc::clone(&finish_barrier);
    manager
        .start_recoverable("run-1".to_string(), move |_cancellation| async move {
            task_finish_barrier.wait().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    finish_barrier.wait().await;
    assert_eq!(
        manager.pause_run("run-1").await,
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn pause_is_the_first_cause_despite_later_shutdown() {
    let manager = WorkflowTaskManager::default();
    let release_cleanup = Arc::new(Notify::new());
    let task_release_cleanup = Arc::clone(&release_cleanup);
    let (cause_tx, cause_rx) = oneshot::channel();
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            let cause = cancellation.cancelled().await;
            cancellation.resolve_pause(/*paused*/ true);
            cause_tx.send(cause).expect("report cancellation cause");
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let pause_manager = manager.clone();
    let pause = tokio::spawn(async move { pause_manager.pause_run("run-1").await });
    assert_eq!(
        cause_rx.await.expect("task observes cancellation"),
        WorkflowCancellationCause::Pause
    );
    let shutdown_manager = manager.clone();
    let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });
    tokio::task::yield_now().await;
    release_cleanup.notify_one();

    assert_eq!(
        pause.await.expect("pause joins cleanup"),
        WorkflowTaskCancelOutcome::Applied
    );
    shutdown.await.expect("shutdown joins cleanup");
}

#[tokio::test]
async fn shutdown_is_the_first_cause_and_late_pause_reports_not_running() {
    let manager = WorkflowTaskManager::default();
    let release_cleanup = Arc::new(Notify::new());
    let task_release_cleanup = Arc::clone(&release_cleanup);
    let (cause_tx, cause_rx) = oneshot::channel();
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            cause_tx
                .send(cancellation.cancelled().await)
                .expect("report cancellation cause");
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let shutdown_manager = manager.clone();
    let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });
    assert_eq!(
        cause_rx.await.expect("task observes cancellation"),
        WorkflowCancellationCause::Interrupted
    );
    let pause_manager = manager.clone();
    let pause = tokio::spawn(async move { pause_manager.pause_run("run-1").await });
    tokio::task::yield_now().await;
    release_cleanup.notify_one();

    assert_eq!(
        pause.await.expect("late pause joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    shutdown.await.expect("shutdown joins cleanup");
}

#[tokio::test]
async fn cancellation_can_race_natural_completion() {
    let manager = WorkflowTaskManager::default();
    let finish_barrier = Arc::new(tokio::sync::Barrier::new(2));
    let task_finish_barrier = Arc::clone(&finish_barrier);
    manager
        .start_recoverable("run-1".to_string(), move |_cancellation| async move {
            task_finish_barrier.wait().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    finish_barrier.wait().await;
    let outcome = manager.cancel_run("run-1").await;
    assert_eq!(outcome, WorkflowTaskCancelOutcome::NotRunning);
    assert_eq!(
        manager.cancel_run("run-1").await,
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn concurrent_stop_requests_both_report_not_running_when_natural_terminal_won() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            assert_eq!(
                cancellation.cancelled().await,
                WorkflowCancellationCause::UserStop
            );
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            cancellation.resolve_user_stop(/*stopped*/ false);
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let first_manager = manager.clone();
    let first = tokio::spawn(async move { first_manager.cancel_run("run-1").await });
    cancellation_seen.notified().await;
    let second_manager = manager.clone();
    let second = tokio::spawn(async move { second_manager.cancel_run("run-1").await });
    tokio::task::yield_now().await;
    release_cleanup.notify_one();
    assert_eq!(
        first.await.expect("first stop joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    assert_eq!(
        second.await.expect("second stop joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn cancellation_during_shutdown_joins_the_same_cleanup() {
    let manager = WorkflowTaskManager::default();
    let cancellation_seen = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task_release_cleanup = Arc::clone(&release_cleanup);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            cancellation.cancelled().await;
            task_cancellation_seen.notify_one();
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let shutdown_manager = manager.clone();
    let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });
    cancellation_seen.notified().await;

    let cancel_manager = manager.clone();
    let cancel = tokio::spawn(async move { cancel_manager.cancel_run("run-1").await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    assert!(!cancel.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        cancel.await.expect("cancellation joins"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    shutdown.await.expect("shutdown joins");
}

#[tokio::test]
async fn user_stop_is_the_first_cancellation_cause_despite_later_shutdown() {
    let manager = WorkflowTaskManager::default();
    let release_cleanup = Arc::new(Notify::new());
    let task_release_cleanup = Arc::clone(&release_cleanup);
    let (cause_tx, cause_rx) = oneshot::channel();
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            let cause = cancellation.cancelled().await;
            cancellation.resolve_user_stop(/*stopped*/ true);
            cause_tx.send(cause).expect("report cancellation cause");
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let cancel_manager = manager.clone();
    let cancel = tokio::spawn(async move { cancel_manager.cancel_run("run-1").await });
    assert_eq!(
        cause_rx.await.expect("task observes cancellation"),
        WorkflowCancellationCause::UserStop
    );

    let shutdown_manager = manager.clone();
    let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });
    tokio::task::yield_now().await;
    assert!(!cancel.is_finished());
    assert!(!shutdown.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        cancel.await.expect("user stop joins cleanup"),
        WorkflowTaskCancelOutcome::Applied
    );
    shutdown.await.expect("later shutdown joins cleanup");
}

#[tokio::test]
async fn shutdown_is_the_first_cancellation_cause_despite_later_user_stop() {
    let manager = WorkflowTaskManager::default();
    let release_cleanup = Arc::new(Notify::new());
    let task_release_cleanup = Arc::clone(&release_cleanup);
    let (cause_tx, cause_rx) = oneshot::channel();
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            let cause = cancellation.cancelled().await;
            cause_tx.send(cause).expect("report cancellation cause");
            task_release_cleanup.notified().await;
            Ok::<_, ()>(())
        })
        .expect("task starts");

    let shutdown_manager = manager.clone();
    let shutdown = tokio::spawn(async move { shutdown_manager.shutdown().await });
    assert_eq!(
        cause_rx.await.expect("task observes cancellation"),
        WorkflowCancellationCause::Interrupted
    );

    let cancel_manager = manager.clone();
    let cancel = tokio::spawn(async move { cancel_manager.cancel_run("run-1").await });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    assert!(!cancel.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        cancel.await.expect("late user stop joins cleanup"),
        WorkflowTaskCancelOutcome::NotRunning
    );
    shutdown.await.expect("shutdown joins cleanup");
}

#[tokio::test]
async fn root_and_admission_cancellation_are_interrupted() {
    let root = CancellationToken::new();
    let child = WorkflowCancellation::child_of(&root);
    root.cancel();
    assert_eq!(
        child.cancelled().await,
        WorkflowCancellationCause::Interrupted
    );

    let admission = WorkflowCancellation::pre_cancelled(WorkflowCancellationCause::Interrupted);
    assert_eq!(
        admission.cancelled().await,
        WorkflowCancellationCause::Interrupted
    );
}

#[tokio::test]
async fn natural_completion_removes_the_run_before_a_stop_can_relabel_it() {
    let manager = WorkflowTaskManager::default();
    let completed = Arc::new(Notify::new());
    let task_completed = Arc::clone(&completed);
    manager
        .start_recoverable("run-1".to_string(), move |cancellation| async move {
            assert!(!cancellation.token.is_cancelled());
            task_completed.notify_one();
            Ok::<_, ()>(())
        })
        .expect("task starts");
    completed.notified().await;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !manager.lock_state().tasks.contains_key("run-1") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("naturally completed task is removed");
    assert_eq!(
        manager.cancel_run("run-1").await,
        WorkflowTaskCancelOutcome::NotRunning
    );
    manager.shutdown().await;
}

async fn wait_until_run_id_can_be_reused(manager: &WorkflowTaskManager, run_id: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match manager.start_recoverable(run_id.to_string(), |_cancellation| async {
                Ok::<_, ()>(())
            }) {
                Ok(_) => break,
                Err(rejected) => {
                    let error = start_error(rejected);
                    if error == WorkflowTaskStartError::AlreadyRunning {
                        tokio::task::yield_now().await;
                    } else {
                        panic!("unexpected task rejection: {error}");
                    }
                }
            }
        }
    })
    .await
    .expect("panicking task is removed");
}

#[tokio::test]
async fn panicking_tasks_are_removed() {
    let manager = WorkflowTaskManager::default();
    let started = Arc::new(Notify::new());
    let task_started = Arc::clone(&started);
    manager
        .start_recoverable::<(), (), _, _>("run-1".to_string(), move |_cancellation| async move {
            task_started.notify_one();
            panic!("fixture panic");
        })
        .expect("task starts");

    started.notified().await;
    wait_until_run_id_can_be_reused(&manager, "run-1").await;
    manager.shutdown().await;
}

#[tokio::test]
async fn panics_while_constructing_the_task_are_also_cleaned_up() {
    let manager = WorkflowTaskManager::default();
    manager
        .start_recoverable::<(), (), _, std::future::Ready<Result<(), ()>>>(
            "run-1".to_string(),
            |_cancellation| panic!("fixture construction panic"),
        )
        .expect("task registers");

    wait_until_run_id_can_be_reused(&manager, "run-1").await;
    manager.shutdown().await;
}
