use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;

use super::TurnStartLifecycleProgress;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::turn_lifecycle::TurnStartDriver;

const TEST_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct LifecycleProbe {
    start_calls: AtomicUsize,
    abort_calls: AtomicUsize,
    abort_completions: AtomicUsize,
    block_start: bool,
    block_first_abort: bool,
    start_entered: Notify,
    abort_entered: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for LifecycleProbe {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.start_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
            if self.block_start {
                self.start_entered.notify_one();
                std::future::pending::<()>().await;
            }
        })
    }

    fn on_turn_abort<'a>(
        &'a self,
        _input: codex_extension_api::TurnAbortInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let call = self.abort_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
            if self.block_first_abort && call == 0 {
                self.abort_entered.notify_one();
                std::future::pending::<()>().await;
            }
            self.abort_completions
                .fetch_add(/*val*/ 1, Ordering::SeqCst);
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ProbeSnapshot {
    start_calls: usize,
    abort_calls: usize,
    abort_completions: usize,
}

impl LifecycleProbe {
    fn snapshot(&self) -> ProbeSnapshot {
        ProbeSnapshot {
            start_calls: self.start_calls.load(Ordering::SeqCst),
            abort_calls: self.abort_calls.load(Ordering::SeqCst),
            abort_completions: self.abort_completions.load(Ordering::SeqCst),
        }
    }
}

async fn make_session(probes: &[Arc<LifecycleProbe>]) -> (Arc<Session>, Arc<TurnContext>) {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    for probe in probes {
        builder.turn_lifecycle_contributor(probe.clone());
    }
    session.services.extensions = Arc::new(builder.build());
    (Arc::new(session), Arc::new(turn_context))
}

async fn begin_start(session: &Session) -> TurnStartDriver {
    let Ok(driver) = session
        .active_turn
        .lock()
        .await
        .begin_fresh_start(/*execution_guard*/ None)
    else {
        panic!("idle session should accept an exact task start");
    };
    driver
}

#[tokio::test]
async fn cancellation_aborts_only_callbacks_entered_during_start() {
    let blocking = Arc::new(LifecycleProbe {
        block_start: true,
        ..Default::default()
    });
    let later = Arc::new(LifecycleProbe::default());
    let (session, turn_context) = make_session(&[Arc::clone(&blocking), Arc::clone(&later)]).await;
    let driver = begin_start(session.as_ref()).await;
    let generation = driver.generation();
    let mut progress = TurnStartLifecycleProgress::default();
    let token_usage = TokenUsage::default();

    {
        let start = session.emit_cancellable_turn_start_lifecycle(
            turn_context.as_ref(),
            &token_usage,
            &generation,
            &mut progress,
        );
        tokio::pin!(start);
        tokio::select! {
            result = timeout(TEST_TIMEOUT, blocking.start_entered.notified()) => {
                result.expect("start callback should be entered");
            }
            _ = &mut start => panic!("blocking start callback returned unexpectedly"),
        }
        assert!(
            session
                .active_turn
                .lock()
                .await
                .cancel_start_exact(&generation, TurnAbortReason::Interrupted)
        );
        timeout(TEST_TIMEOUT, &mut start)
            .await
            .expect("start lifecycle should observe exact cancellation");
    }

    assert_eq!(
        progress,
        TurnStartLifecycleProgress {
            entered: 1,
            next_abort: 0,
        }
    );
    session
        .emit_entered_turn_abort_lifecycle(
            TurnAbortReason::Interrupted,
            turn_context.extension_data.as_ref(),
            &mut progress,
        )
        .await;
    assert_eq!(
        vec![blocking.snapshot(), later.snapshot()],
        vec![
            ProbeSnapshot {
                start_calls: 1,
                abort_calls: 1,
                abort_completions: 1,
            },
            ProbeSnapshot {
                start_calls: 0,
                abort_calls: 0,
                abort_completions: 0,
            },
        ]
    );
    let Ok(reason) = session
        .active_turn
        .lock()
        .await
        .complete_cancelled_start(driver)
    else {
        panic!("exact start driver should complete compensation");
    };
    assert_eq!(reason, TurnAbortReason::Interrupted);
}

#[tokio::test]
async fn cancelled_abort_retries_only_the_inflight_contributor() {
    let first = Arc::new(LifecycleProbe::default());
    let second = Arc::new(LifecycleProbe {
        block_first_abort: true,
        ..Default::default()
    });
    let third = Arc::new(LifecycleProbe::default());
    let probes = [Arc::clone(&first), Arc::clone(&second), Arc::clone(&third)];
    let (session, turn_context) = make_session(&probes).await;
    let driver = begin_start(session.as_ref()).await;
    let generation = driver.generation();
    let mut progress = TurnStartLifecycleProgress::default();
    session
        .emit_cancellable_turn_start_lifecycle(
            turn_context.as_ref(),
            &TokenUsage::default(),
            &generation,
            &mut progress,
        )
        .await;
    assert!(
        session
            .active_turn
            .lock()
            .await
            .cancel_start_exact(&generation, TurnAbortReason::Interrupted)
    );

    {
        let abort = session.emit_entered_turn_abort_lifecycle(
            TurnAbortReason::Interrupted,
            turn_context.extension_data.as_ref(),
            &mut progress,
        );
        tokio::pin!(abort);
        tokio::select! {
            result = timeout(TEST_TIMEOUT, second.abort_entered.notified()) => {
                result.expect("second abort callback should be entered");
            }
            _ = &mut abort => panic!("blocking abort callback returned unexpectedly"),
        }
    }
    assert_eq!(
        progress,
        TurnStartLifecycleProgress {
            entered: 3,
            next_abort: 1,
        }
    );

    timeout(
        TEST_TIMEOUT,
        session.emit_entered_turn_abort_lifecycle(
            TurnAbortReason::Interrupted,
            turn_context.extension_data.as_ref(),
            &mut progress,
        ),
    )
    .await
    .expect("resumed abort lifecycle should finish");
    assert_eq!(
        probes.map(|probe| probe.snapshot()),
        [
            ProbeSnapshot {
                start_calls: 1,
                abort_calls: 1,
                abort_completions: 1,
            },
            ProbeSnapshot {
                start_calls: 1,
                abort_calls: 2,
                abort_completions: 1,
            },
            ProbeSnapshot {
                start_calls: 1,
                abort_calls: 1,
                abort_completions: 1,
            },
        ]
    );
    let Ok(reason) = session
        .active_turn
        .lock()
        .await
        .complete_cancelled_start(driver)
    else {
        panic!("exact start driver should complete compensation");
    };
    assert_eq!(reason, TurnAbortReason::Interrupted);
}
