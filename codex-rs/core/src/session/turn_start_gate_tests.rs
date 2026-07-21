use super::TurnStartGate;

#[test]
fn close_is_permanent() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    assert!(gate.is_open());

    gate.close();
    assert!(!gate.is_open());
    assert_eq!(gate.automatic_start_ticket(), None);
    assert!(!gate.admits_automatic_start(ticket));
    assert_eq!(gate.retry_ticket_after_invalidation(ticket), None);

    let retry = gate.retry_automatic_starts_after_invalidation();
    assert!(!gate.admits_automatic_start(retry));

    gate.close();
    assert!(!gate.is_open());
}

#[test]
fn automatic_start_tickets_are_invalidated_without_closing_user_starts() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");

    let suppressed = gate.suppress_automatic_starts();

    assert!(gate.is_open());
    assert!(!gate.admits_automatic_start(ticket));
    assert_eq!(gate.retry_ticket_after_invalidation(ticket), None);
    let current = gate.automatic_start_ticket().expect("gate remains open");
    assert_eq!(current, suppressed);
    assert!(gate.admits_automatic_start(current));
}

#[test]
fn interrupted_automatic_start_can_refresh_exactly_to_the_current_epoch() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");

    let retry_from_invalidation = gate.retry_automatic_starts_after_invalidation();

    let retry = gate
        .retry_ticket_after_invalidation(ticket)
        .expect("interrupt invalidation should permit retry");
    assert_eq!(retry, retry_from_invalidation);
    assert!(gate.admits_automatic_start(retry));
    assert_eq!(gate.retry_ticket_after_invalidation(retry), None);
}

#[test]
fn stale_interrupt_ticket_cannot_skip_a_later_suppression() {
    let gate = TurnStartGate::default();
    let original = gate.automatic_start_ticket().expect("gate open");

    let interrupted = gate.retry_automatic_starts_after_invalidation();
    gate.suppress_automatic_starts();

    assert_eq!(gate.retry_ticket_after_invalidation(original), None);
    assert_eq!(gate.retry_ticket_after_invalidation(interrupted), None);
}

#[test]
fn retry_refresh_cannot_skip_an_intermediate_retry_epoch() {
    let gate = TurnStartGate::default();
    let original = gate.automatic_start_ticket().expect("gate open");
    let first_retry = gate.retry_automatic_starts_after_invalidation();
    let second_retry = gate.retry_automatic_starts_after_invalidation();

    assert_eq!(gate.retry_ticket_after_invalidation(original), None);
    assert_eq!(
        gate.retry_ticket_after_invalidation(first_retry),
        Some(second_retry)
    );
}

#[test]
fn current_suppressed_epoch_can_follow_its_immediate_interrupt() {
    let gate = TurnStartGate::default();
    let suppressed = gate.suppress_automatic_starts();
    let interrupted = gate.retry_automatic_starts_after_invalidation();

    assert_eq!(
        gate.retry_ticket_after_invalidation(suppressed),
        Some(interrupted)
    );
}
