//! Tests for the workflow concurrency scheduler (P1-scheduler-semaphore, spec §5).
//!
//! The spawn keystone (`spawn_and_await_final_message`) needs a live `Session` /
//! `AgentControl`, so these tests exercise the scheduler through instrumented spawn
//! fixtures — closures that stall, count in-flight admissions, or simulate the registry
//! backstop's `AgentLimitReached` — which is exactly what the acceptance criteria call
//! for ("an instrumented fixture that stalls children").

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tokio::sync::Semaphore;

use super::AgentCapReached;
use super::LIFETIME_SPAWN_CAP;
use super::SpawnAttempt;
use super::WorkflowScheduler;
use super::normalize_workflow_concurrency;
use super::policy_cap_from_cores;
use super::workflow_concurrency_cap_from;

/// Tracks live/peak concurrent admissions so a test can assert in-flight never exceeds
/// the cap.
#[derive(Default)]
struct InFlightGauge {
    live: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
}

impl InFlightGauge {
    fn enter(&self) {
        let now = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(now, Ordering::AcqRel);
    }

    fn leave(&self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        self.completed.fetch_add(1, Ordering::AcqRel);
    }

    fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::Acquire)
    }
}

/// Spin (cooperatively, no wall-clock) until `cond` holds, asserting the invariant
/// `gauge.live() <= cap` on every poll so an over-admission is caught the instant it
/// happens. Bounded so a hang fails loudly instead of spinning forever.
async fn wait_until(gauge: &InFlightGauge, cap: usize, cond: impl Fn() -> bool) {
    for _ in 0..1_000_000 {
        assert!(
            gauge.live() <= cap,
            "in-flight {} exceeded cap {cap}",
            gauge.live()
        );
        if cond() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not reached; live={} cap={cap}", gauge.live());
}

// -- cap policy ------------------------------------------------------------------

#[test]
fn policy_cap_is_min_16_cores_minus_2() {
    // min(16, cores-2), floored at 1.
    assert_eq!(policy_cap_from_cores(1), 1);
    assert_eq!(policy_cap_from_cores(2), 1);
    assert_eq!(policy_cap_from_cores(3), 1);
    assert_eq!(policy_cap_from_cores(4), 2);
    assert_eq!(policy_cap_from_cores(10), 8);
    // Saturates at 16 no matter how many cores.
    assert_eq!(policy_cap_from_cores(18), 16);
    assert_eq!(policy_cap_from_cores(64), 16);
}

#[test]
fn workflow_clamp_is_raised_so_policy_cap_wins() {
    // With the session-default effective_agent_max_threads (6, well below the
    // min(16,cores-2) ceiling) the workflow-raised clamp must NOT pull the cap below
    // the policy value — it is raised to the workflow ceiling.
    assert_eq!(workflow_concurrency_cap_from(10, Some(6)), 8); // policy min(16,8)=8 wins
    assert_eq!(workflow_concurrency_cap_from(18, Some(6)), 16); // reaches the 16 ceiling
    assert_eq!(workflow_concurrency_cap_from(18, None), 16);
    // A single-core-ish box still admits one.
    assert_eq!(workflow_concurrency_cap_from(2, Some(6)), 1);
}

#[test]
fn normalize_clamp_is_actually_applied() {
    // The clamp genuinely lowers the cap when it is smaller than the policy cap — the
    // workflow override simply keeps it >= policy for real runs, but the primitive
    // still clamps.
    assert_eq!(normalize_workflow_concurrency(8, 4), 4);
    assert_eq!(normalize_workflow_concurrency(8, 16), 8);
    // Never returns 0.
    assert_eq!(normalize_workflow_concurrency(8, 0), 1);
    assert_eq!(normalize_workflow_concurrency(0, 4), 1);
}

// -- admission / concurrency bound -----------------------------------------------

/// Instrumented stalled-children fixture: N admissions each stall on a gate until the
/// test releases it, proving the number of *concurrently in-flight* children never
/// exceeds the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_never_exceeds_cap_with_stalled_children() {
    const CAP: usize = 3;
    const N: usize = 12;

    let scheduler = WorkflowScheduler::with_cap(CAP);
    assert_eq!(scheduler.cap(), CAP);

    let gauge = Arc::new(InFlightGauge::default());
    // A gate with 0 permits: every child blocks here, so admitted children stay
    // in-flight until the test hands out permits.
    let gate = Arc::new(Semaphore::new(0));

    let mut handles = Vec::new();
    for _ in 0..N {
        let scheduler = scheduler.clone();
        let gauge = Arc::clone(&gauge);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            scheduler
                .admit(|| {
                    let gauge = Arc::clone(&gauge);
                    let gate = Arc::clone(&gate);
                    async move {
                        gauge.enter();
                        // Stall while "running" — this is the child that never returns
                        // until released.
                        let _permit = gate.acquire().await.expect("gate open");
                        gauge.leave();
                        SpawnAttempt::Finalized(())
                    }
                })
                .await
                .expect("within lifetime cap");
        }));
    }

    // Exactly CAP children must become in-flight and then STAY there (the rest queue on
    // the semaphore). Assert we reach the cap and never blow past it.
    wait_until(&gauge, CAP, || gauge.live() == CAP).await;
    // Give any erroneously-admitted extra child a chance to appear, then re-check.
    for _ in 0..1000 {
        assert!(gauge.live() <= CAP, "over-admitted: {}", gauge.live());
        tokio::task::yield_now().await;
    }
    assert_eq!(gauge.live(), CAP, "the cap should be fully saturated");

    // Release everything and drain.
    gate.add_permits(N);
    for handle in handles {
        handle.await.expect("admit task");
    }

    assert!(
        gauge.peak() <= CAP,
        "peak in-flight {} > cap {CAP}",
        gauge.peak()
    );
    assert_eq!(
        gauge.peak(),
        CAP,
        "the fixture should have saturated the cap"
    );
    assert_eq!(gauge.completed(), N, "every admission must complete");
    assert_eq!(scheduler.available_permits(), CAP, "all permits released");
}

/// The acceptance headline: a 32-way `parallel()` with cap=8 admits at most 8 at once;
/// the remainder queue and all 32 complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_32_with_cap_8_admits_at_most_8_and_all_complete() {
    const CAP: usize = 8;
    const N: usize = 32;

    let scheduler = WorkflowScheduler::with_cap(CAP);
    let gauge = Arc::new(InFlightGauge::default());
    let gate = Arc::new(Semaphore::new(0));

    let mut handles = Vec::new();
    for i in 0..N {
        let scheduler = scheduler.clone();
        let gauge = Arc::clone(&gauge);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            scheduler
                .admit(|| {
                    let gauge = Arc::clone(&gauge);
                    let gate = Arc::clone(&gate);
                    async move {
                        gauge.enter();
                        let _permit = gate.acquire().await.expect("gate open");
                        gauge.leave();
                        SpawnAttempt::Finalized(i)
                    }
                })
                .await
                .expect("within lifetime cap")
        }));
    }

    // First wave: exactly 8 in-flight, 24 queued.
    wait_until(&gauge, CAP, || gauge.live() == CAP).await;
    assert_eq!(gauge.live(), CAP);

    // Drip-release in batches so the queued remainder is admitted in waves; the peak
    // must still never exceed the cap.
    for _ in 0..N {
        gate.add_permits(1);
        assert!(
            gauge.live() <= CAP,
            "over cap during drain: {}",
            gauge.live()
        );
        tokio::task::yield_now().await;
    }

    let mut results: Vec<usize> = Vec::new();
    for handle in handles {
        results.push(handle.await.expect("admit task"));
    }
    results.sort_unstable();
    assert_eq!(results, (0..N).collect::<Vec<_>>(), "all 32 complete");
    assert!(gauge.peak() <= CAP, "peak {} > cap {CAP}", gauge.peak());
    assert_eq!(gauge.peak(), CAP, "the batch should have saturated the cap");
    assert_eq!(gauge.completed(), N);
    assert_eq!(scheduler.available_permits(), CAP);
}

// -- permit release on both paths ------------------------------------------------

#[tokio::test]
async fn permit_released_on_success_finalize() {
    let scheduler = WorkflowScheduler::with_cap(2);
    assert_eq!(scheduler.available_permits(), 2);
    // Success path: agent() returns Some(text).
    let out = scheduler
        .admit(|| async { SpawnAttempt::Finalized(Some("final text".to_string())) })
        .await
        .expect("within lifetime cap");
    assert_eq!(out.as_deref(), Some("final text"));
    assert_eq!(
        scheduler.available_permits(),
        2,
        "permit released on success"
    );
}

#[tokio::test]
async fn permit_released_on_failure_abort_finalize() {
    let scheduler = WorkflowScheduler::with_cap(2);
    // Failure/abort path: agent() resolves to None (TurnAborted / spawn error). It is
    // still a `Finalized` outcome, so the permit must be released just the same.
    let out: Option<String> = scheduler
        .admit(|| async { SpawnAttempt::Finalized(None) })
        .await
        .expect("within lifetime cap");
    assert!(out.is_none());
    assert_eq!(
        scheduler.available_permits(),
        2,
        "permit released on failure/abort finalize"
    );
}

// -- requeue on AgentLimitReached ------------------------------------------------

#[tokio::test]
async fn agent_limit_reached_requeues_rather_than_erroring() {
    let scheduler = WorkflowScheduler::with_cap(4);
    // The registry backstop rejects the first 3 attempts, then admits. The scheduler
    // must keep requeuing and ultimately finalize — never surface an error.
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_spawn = Arc::clone(&attempts);
    let out = scheduler
        .admit(move || {
            let attempts = Arc::clone(&attempts_for_spawn);
            async move {
                let n = attempts.fetch_add(1, Ordering::AcqRel);
                if n < 3 {
                    SpawnAttempt::AgentLimitReached
                } else {
                    SpawnAttempt::Finalized(n)
                }
            }
        })
        .await
        .expect("within lifetime cap");
    assert_eq!(out, 3, "finalized on the 4th attempt");
    assert_eq!(attempts.load(Ordering::Acquire), 4, "requeued three times");
    assert_eq!(
        scheduler.available_permits(),
        4,
        "permit released after each requeue and on finalize"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requeue_does_not_block_siblings_while_backstop_saturated() {
    // A call stuck requeuing on AgentLimitReached must not hold a permit — a sibling
    // admission has to be able to run to completion meanwhile. If the requeue held its
    // permit, cap-1 concurrency would strand this sibling on a cap-1 scheduler.
    let scheduler = WorkflowScheduler::with_cap(1);
    let sibling_done = Arc::new(Semaphore::new(0));

    let requeuing = {
        let scheduler = scheduler.clone();
        let sibling_done = Arc::clone(&sibling_done);
        tokio::spawn(async move {
            scheduler
                .admit(|| {
                    let sibling_done = Arc::clone(&sibling_done);
                    async move {
                        // Keep requeuing until the sibling has finished. Because the
                        // permit is dropped before each requeue, the sibling below can
                        // acquire the single permit and finish.
                        if sibling_done.available_permits() == 0 {
                            SpawnAttempt::AgentLimitReached
                        } else {
                            SpawnAttempt::Finalized(())
                        }
                    }
                })
                .await
                .expect("within lifetime cap");
        })
    };

    // Sibling must be able to acquire the one permit and finalize despite the requeue.
    scheduler
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect("within lifetime cap");
    sibling_done.add_permits(1);

    requeuing
        .await
        .expect("requeuing admit completes once backstop clears");
    assert_eq!(scheduler.available_permits(), 1, "all permits released");
}

// -- lifetime cap (spec §5, ceiling 1000, monotonic) -----------------------------

#[tokio::test]
async fn admission_1001_throws_agent_cap_reached() {
    // The headline acceptance: with the real per-run ceiling, exactly 1000 admissions
    // succeed and the 1001st throws AgentCapReached. Concurrency cap is small; each spawn
    // finalizes immediately so the run reaches the lifetime boundary, not a permit stall.
    let scheduler = WorkflowScheduler::with_cap(4);
    assert_eq!(scheduler.lifetime_cap(), LIFETIME_SPAWN_CAP);

    for _ in 0..LIFETIME_SPAWN_CAP {
        scheduler
            .admit(|| async { SpawnAttempt::Finalized(()) })
            .await
            .expect("admission within lifetime cap");
    }
    assert_eq!(scheduler.lifetime_spawned(), LIFETIME_SPAWN_CAP);

    let err = scheduler
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect_err("1001st admission throws AgentCapReached");
    assert_eq!(
        err,
        AgentCapReached {
            cap: LIFETIME_SPAWN_CAP
        }
    );
    assert_eq!(
        scheduler.lifetime_spawned(),
        LIFETIME_SPAWN_CAP,
        "the rejected admission does not advance the counter past the cap"
    );
}

#[tokio::test]
async fn completed_agents_do_not_free_lifetime_budget() {
    // Admit exactly `lifetime_cap` agents, each finalizing (which RELEASES its concurrency
    // permit). Despite every one completing and returning its permit, the monotonic
    // counter never decrements, so the next admission still throws — completed/released
    // agents do not give lifetime budget back.
    const LIFE: usize = 3;
    let scheduler = WorkflowScheduler::with_caps(2, LIFE);

    for _ in 0..LIFE {
        scheduler
            .admit(|| async { SpawnAttempt::Finalized(()) })
            .await
            .expect("within lifetime cap");
    }
    // Every agent finalized, so all concurrency permits are back ...
    assert_eq!(
        scheduler.available_permits(),
        2,
        "concurrency permits released on finalize"
    );
    // ... but the lifetime budget is spent and does not decrement.
    assert_eq!(scheduler.lifetime_spawned(), LIFE);

    let err = scheduler
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect_err("completed agents do not free lifetime budget");
    assert_eq!(err, AgentCapReached { cap: LIFE });
    assert_eq!(
        scheduler.lifetime_spawned(),
        LIFE,
        "the lifetime counter never decrements"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifetime_cap_reached_throws_before_awaiting_permit() {
    // Admission-order proof (step 2 before step 4): the lifetime CAS runs BEFORE the
    // concurrency permit is acquired. Concurrency cap 1 (saturated by a stalled child) and
    // lifetime cap 1: the next admission must return AgentCapReached *immediately* without
    // awaiting the held permit. If the CAS ran after the permit await, this admit would
    // block forever on the permit the stalled child holds, hanging the test.
    let scheduler = WorkflowScheduler::with_caps(1, 1);
    let gauge = Arc::new(InFlightGauge::default());
    let gate = Arc::new(Semaphore::new(0));

    let stalled = {
        let scheduler = scheduler.clone();
        let gauge = Arc::clone(&gauge);
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            scheduler
                .admit(|| {
                    let gauge = Arc::clone(&gauge);
                    let gate = Arc::clone(&gate);
                    async move {
                        gauge.enter();
                        let _permit = gate.acquire().await.expect("gate open");
                        gauge.leave();
                        SpawnAttempt::Finalized(())
                    }
                })
                .await
                .expect("first admit within lifetime cap");
        })
    };

    // Wait until the stalled child holds the only permit and the only lifetime slot.
    wait_until(&gauge, 1, || gauge.live() == 1).await;
    assert_eq!(
        scheduler.available_permits(),
        0,
        "the stalled child holds the only permit"
    );
    assert_eq!(scheduler.lifetime_spawned(), 1);

    // Over the lifetime cap: must return Err immediately despite the permit being
    // unavailable, proving the CAS precedes the permit await. The spawn closure below
    // never runs (no gauge.enter), because the CAS fails first.
    let err = scheduler
        .admit(|| async {
            gauge.enter();
            SpawnAttempt::Finalized(())
        })
        .await
        .expect_err("lifetime cap reached before permit await");
    assert_eq!(err, AgentCapReached { cap: 1 });
    assert_eq!(
        scheduler.lifetime_spawned(),
        1,
        "the rejected admission neither spawned nor advanced the counter"
    );

    // Release the stalled child and drain.
    gate.add_permits(1);
    stalled.await.expect("stalled admit task");
}

#[tokio::test]
async fn second_run_starts_fresh_at_zero() {
    // The lifetime cap is per-run, not per-session: a fresh scheduler is a fresh run with
    // its own counter starting at 0.
    const LIFE: usize = 2;

    let first = WorkflowScheduler::with_caps(2, LIFE);
    for _ in 0..LIFE {
        first
            .admit(|| async { SpawnAttempt::Finalized(()) })
            .await
            .expect("within lifetime cap");
    }
    first
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect_err("first run exhausted its lifetime budget");
    assert_eq!(first.lifetime_spawned(), LIFE);

    // A brand-new scheduler models a second run; it starts fresh and admits again.
    let second = WorkflowScheduler::with_caps(2, LIFE);
    assert_eq!(second.lifetime_spawned(), 0, "a fresh run starts at 0");
    second
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect("second run admits from a fresh count");
    assert_eq!(second.lifetime_spawned(), 1);

    // The two runs are fully independent — the first stays exhausted.
    assert_eq!(first.lifetime_spawned(), LIFE);
}

#[tokio::test]
async fn clones_share_one_per_run_lifetime_budget() {
    // A parallel()/pipeline() fan-out admits through clones of one scheduler; those clones
    // must count against a single per-run lifetime budget (Arc-shared counter), not one
    // budget per clone.
    const LIFE: usize = 2;
    let scheduler = WorkflowScheduler::with_caps(2, LIFE);
    let clone = scheduler.clone();

    scheduler
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect("first admission via the original handle");
    clone
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect("second admission via the cloned handle");

    // Both handles observe the shared count and the shared exhaustion.
    assert_eq!(scheduler.lifetime_spawned(), LIFE);
    assert_eq!(clone.lifetime_spawned(), LIFE);
    let err = clone
        .admit(|| async { SpawnAttempt::Finalized(()) })
        .await
        .expect_err("the shared budget is exhausted across clones");
    assert_eq!(err, AgentCapReached { cap: LIFE });
}

// -- permit RAII on cancellation/drop and panic (spec §5 step 6) ------------------

/// A permit must be released when the admit future is dropped mid-flight (cancellation): the permit
/// is an RAII guard, so a dropped spawn-and-await future frees the slot for its siblings rather than
/// stranding it. Concurrency cap 1 makes the leak unmistakable — a stranded permit would wedge the
/// whole run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permit_released_when_admit_future_is_cancelled_mid_flight() {
    let scheduler = WorkflowScheduler::with_cap(1);
    // A gate that never opens: the admitted child stalls while holding the only permit.
    let gate = Arc::new(Semaphore::new(0));

    let handle = {
        let scheduler = scheduler.clone();
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let _ = scheduler
                .admit(|| {
                    let gate = Arc::clone(&gate);
                    async move {
                        let _permit = gate.acquire().await.expect("gate open");
                        SpawnAttempt::Finalized(())
                    }
                })
                .await;
        })
    };

    // Wait until the admission has acquired the permit and is stalled inside the spawn.
    for _ in 0..1_000_000 {
        if scheduler.available_permits() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        scheduler.available_permits(),
        0,
        "the stalled admission must hold the only permit"
    );

    // Cancel the admit future mid-flight; its permit guard must release the slot on drop.
    handle.abort();
    let _ = handle.await;
    for _ in 0..1_000_000 {
        if scheduler.available_permits() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        scheduler.available_permits(),
        1,
        "a cancelled/dropped admission must release its permit"
    );
}

/// A permit must be released when the spawn closure panics: the permit guard drops during unwind, so
/// a panicking admission never strands the slot. (The lifetime counter is monotonic and stays spent —
/// only the concurrency permit is reclaimed.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permit_released_when_admission_panics() {
    let scheduler = WorkflowScheduler::with_cap(1);

    let handle = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move {
            let _ = scheduler
                .admit(|| async {
                    panic!("boom while spawning the child");
                    #[allow(unreachable_code)]
                    SpawnAttempt::Finalized(())
                })
                .await;
        })
    };

    let joined = handle.await;
    assert!(joined.is_err(), "the admission task must have panicked");
    assert_eq!(
        scheduler.available_permits(),
        1,
        "a panicking admission must release its permit during unwind"
    );
    // The monotonic lifetime slot claimed before the panic is not given back.
    assert_eq!(
        scheduler.lifetime_spawned(),
        1,
        "the lifetime counter never decrements, even on a panicking admission"
    );
}
