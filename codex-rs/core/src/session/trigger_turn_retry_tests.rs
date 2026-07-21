use std::sync::Arc;
use std::sync::Barrier;

use pretty_assertions::assert_eq;

use super::AutomaticTicketInvalidation;
use super::PendingWorkStartRequest;
use super::TriggerTurnRetry;
use crate::session::turn_start_gate::TurnStartGate;

#[test]
fn same_epoch_merge_keeps_latest_request_and_retry_dominates_suppression() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let cases = [
        (
            AutomaticTicketInvalidation::Suppress,
            AutomaticTicketInvalidation::Suppress,
            AutomaticTicketInvalidation::Suppress,
        ),
        (
            AutomaticTicketInvalidation::Suppress,
            AutomaticTicketInvalidation::Retry,
            AutomaticTicketInvalidation::Retry,
        ),
        (
            AutomaticTicketInvalidation::Retry,
            AutomaticTicketInvalidation::Suppress,
            AutomaticTicketInvalidation::Retry,
        ),
        (
            AutomaticTicketInvalidation::Retry,
            AutomaticTicketInvalidation::Retry,
            AutomaticTicketInvalidation::Retry,
        ),
    ];

    for (index, (first, newest, expected)) in cases.into_iter().enumerate() {
        let retry = TriggerTurnRetry::default();
        let claim = retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                format!("first-{index}"),
                ticket,
                first,
            ))
            .expect("first capacity request should own the wait");
        let newest_sub_id = format!("newest-{index}");

        assert!(
            retry
                .register_capacity_wait(PendingWorkStartRequest::new(
                    newest_sub_id.clone(),
                    ticket,
                    newest,
                ))
                .is_none()
        );
        assert_eq!(
            claim.take_request().into_parts(),
            (newest_sub_id, ticket, expected)
        );
    }
}

#[test]
fn newer_authenticated_epoch_replaces_waiting_request() {
    let gate = TurnStartGate::default();
    let old_ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let claim = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "old".to_string(),
            old_ticket,
            AutomaticTicketInvalidation::Retry,
        ))
        .expect("first capacity request should own the wait");
    let current_ticket = gate.suppress_automatic_starts();

    assert!(
        retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                "current".to_string(),
                current_ticket,
                AutomaticTicketInvalidation::Suppress,
            ))
            .is_none()
    );
    let (sub_id, retained_ticket, invalidation) = claim.take_request().into_parts();
    assert_eq!(
        (sub_id, retained_ticket, invalidation),
        (
            "current".to_string(),
            current_ticket,
            AutomaticTicketInvalidation::Suppress,
        )
    );
}

#[test]
fn dropping_capacity_claim_rearms_wait_registration() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let claim = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "first".to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        ))
        .expect("first capacity request should own the wait");

    drop(claim);

    assert!(
        retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                "second".to_string(),
                ticket,
                AutomaticTicketInvalidation::Retry,
            ))
            .is_some()
    );
}

#[test]
fn taking_capacity_request_rearms_wait_registration() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let first = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "first".to_string(),
            ticket,
            AutomaticTicketInvalidation::Suppress,
        ))
        .expect("first capacity request should own the wait");

    assert_eq!(
        first.take_request().into_parts(),
        (
            "first".to_string(),
            ticket,
            AutomaticTicketInvalidation::Suppress,
        )
    );
    let second = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "second".to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        ))
        .expect("taking the request should rearm capacity registration");
    assert_eq!(
        second.take_request().into_parts(),
        (
            "second".to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        )
    );
}

#[test]
fn concurrent_capacity_requests_choose_exactly_one_wait_owner() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = Arc::new(TriggerTurnRetry::default());
    let start = Arc::new(Barrier::new(/*n*/ 3));
    let finish = Arc::new(Barrier::new(/*n*/ 3));
    let handles: [_; 2] = std::array::from_fn(|index| {
        let retry = Arc::clone(&retry);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        std::thread::spawn(move || {
            start.wait();
            let claim = retry.register_capacity_wait(PendingWorkStartRequest::new(
                index.to_string(),
                ticket,
                AutomaticTicketInvalidation::Retry,
            ));
            finish.wait();
            claim
        })
    });

    start.wait();
    finish.wait();
    let mut claims = handles.map(|handle| {
        handle
            .join()
            .expect("capacity retry contender should not panic")
    });
    claims.sort_by_key(Option::is_some);
    assert!(claims[0].is_none());
    assert!(claims[1].is_some());
}
