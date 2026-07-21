use std::sync::Arc;
use std::sync::Barrier;

use pretty_assertions::assert_eq;

use super::TriggerTurnRetry;

#[test]
fn wait_claim_releases_before_retry_and_rearms_on_drop() {
    let retry = TriggerTurnRetry::default();
    let first = retry
        .try_begin_wait()
        .expect("first capacity wait should drive the retry");

    assert!(retry.try_begin_wait().is_none());
    first.release_for_retry();

    let second = retry
        .try_begin_wait()
        .expect("release should rearm before retrying admission");
    assert!(retry.try_begin_wait().is_none());
    drop(second);

    assert!(retry.try_begin_wait().is_some());
}

#[test]
fn concurrent_wait_claims_choose_exactly_one_driver() {
    let retry = Arc::new(TriggerTurnRetry::default());
    let start = Arc::new(Barrier::new(/*n*/ 3));
    let finish = Arc::new(Barrier::new(/*n*/ 3));
    let handles: [_; 2] = std::array::from_fn(|_| {
        let retry = Arc::clone(&retry);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        std::thread::spawn(move || {
            start.wait();
            let claim = retry.try_begin_wait();
            finish.wait();
            claim
        })
    });

    start.wait();
    finish.wait();
    let mut claimed = handles.map(|handle| {
        handle
            .join()
            .expect("capacity retry contender should not panic")
            .is_some()
    });
    claimed.sort_unstable();
    assert_eq!(claimed, [false, true]);

    assert!(retry.try_begin_wait().is_some());
}
