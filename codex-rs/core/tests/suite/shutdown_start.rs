use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use codex_core::TryStartTurnIfIdleRejectionReason;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use core_test_support::responses;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct PauseFirstTurnStart {
    calls: AtomicUsize,
    first_entered: Notify,
    first_release: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for PauseFirstTurnStart {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if self.calls.fetch_add(/*val*/ 1, Ordering::SeqCst) == 0 {
                self.first_entered.notify_one();
                self.first_release.notified().await;
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_public_start_prevents_commit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let mut extension_builder = ExtensionRegistryBuilder::<Config>::new();
    extension_builder.turn_lifecycle_contributor(probe.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extension_builder.build()))
        .build_with_auto_env(&server)
        .await?;
    let input = vec![responses::user_message_item("shutdown start")];
    let input_for_start = input.clone();
    let codex_for_start = Arc::clone(&test.codex);
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(input_for_start)
            .await
    });
    timeout(TEST_TIMEOUT, probe.first_entered.notified())
        .await
        .expect("turn should enter its start callback");

    timeout(TEST_TIMEOUT, test.codex.shutdown_and_wait())
        .await
        .expect("shutdown should not deadlock with an in-flight start")?;
    let rejected = timeout(TEST_TIMEOUT, starting)
        .await
        .expect("shutdown start should terminalize")
        .expect("shutdown start task should not panic")
        .expect_err("shutdown should reject an uncommitted automatic start");
    assert_eq!(TryStartTurnIfIdleRejectionReason::Busy, rejected.reason());
    assert_eq!(input, rejected.into_input());
    assert_eq!(1, probe.calls.load(Ordering::SeqCst));

    let after_shutdown = vec![responses::user_message_item("after shutdown")];
    let rejected = test
        .codex
        .try_start_turn_if_idle(after_shutdown.clone())
        .await
        .expect_err("shutdown must permanently close direct turn starts");
    assert_eq!(TryStartTurnIfIdleRejectionReason::Busy, rejected.reason());
    assert_eq!(after_shutdown, rejected.into_input());

    let response_request_count = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .count();
    assert_eq!(0, response_request_count);
    Ok(())
}
