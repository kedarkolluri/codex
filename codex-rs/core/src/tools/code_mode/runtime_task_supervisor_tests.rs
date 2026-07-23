use super::RuntimeTaskSupervisor;
use std::sync::Arc;
use std::sync::Barrier;

#[tokio::test]
async fn shutdown_cancels_reserved_tasks_and_rejects_new_admission() {
    let supervisor = RuntimeTaskSupervisor::new();
    let permit = supervisor.reserve().expect("reserve task");
    let cancellation = permit.cancellation_token();
    assert_eq!(supervisor.tasks.len(), 1);

    supervisor.begin_shutdown();

    assert!(cancellation.is_cancelled());
    assert!(supervisor.reserve().is_none());
    let wait = supervisor.wait();
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());
    drop(permit);
    wait.await;
    assert!(supervisor.tasks.is_empty());
}

#[test]
fn admission_racing_shutdown_is_either_rejected_or_tracked() {
    let supervisor = Arc::new(RuntimeTaskSupervisor::new());
    let ready = Arc::new(Barrier::new(/*n*/ 3));
    let reserve_supervisor = Arc::clone(&supervisor);
    let reserve_ready = Arc::clone(&ready);
    let reserve = std::thread::spawn(move || {
        reserve_ready.wait();
        reserve_supervisor.reserve()
    });
    let shutdown_supervisor = Arc::clone(&supervisor);
    let shutdown_ready = Arc::clone(&ready);
    let shutdown = std::thread::spawn(move || {
        shutdown_ready.wait();
        shutdown_supervisor.begin_shutdown();
    });

    ready.wait();
    let permit = reserve.join().expect("reserve thread");
    shutdown.join().expect("shutdown thread");

    assert!(supervisor.is_shutting_down());
    assert!(supervisor.reserve().is_none());
    if let Some(permit) = permit {
        assert!(permit.cancellation_token().is_cancelled());
        assert_eq!(supervisor.tasks.len(), 1);
        drop(permit);
    }
    assert!(supervisor.tasks.is_empty());
}

#[test]
fn dropping_the_supervisor_cancels_existing_reservations() {
    let supervisor = RuntimeTaskSupervisor::new();
    let permit = supervisor.reserve().expect("reserve task");
    let cancellation = permit.cancellation_token();

    drop(supervisor);

    assert!(cancellation.is_cancelled());
    drop(permit);
}
