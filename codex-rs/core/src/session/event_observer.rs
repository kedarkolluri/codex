use std::sync::OnceLock;

use codex_protocol::protocol::Event;
use tokio::sync::broadcast;

use super::Session;

/// Maximum number of delivered events retained for non-competing observers.
///
/// Observers are expected to reconcile a lagged stream against session status instead of relying
/// on unbounded buffering.
pub(super) const EVENT_OBSERVER_CAPACITY: usize = 1024;

#[derive(Default)]
pub(super) struct EventObserverTap {
    sender: OnceLock<broadcast::Sender<Event>>,
}

impl EventObserverTap {
    pub(super) fn deliver(&self, event: &Event) {
        if let Some(sender) = self.sender.get()
            && sender.receiver_count() > 0
        {
            let _ = sender.send(event.clone());
        }
    }

    // Used by the next stacked workflow spawn-and-await consumer.
    #[allow(dead_code)]
    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender
            .get_or_init(|| broadcast::channel(EVENT_OBSERVER_CAPACITY).0)
            .subscribe()
    }
}

impl Session {
    /// Subscribe to a bounded, non-competing tap of events routed through session delivery.
    ///
    /// Unlike the primary MPMC event receiver, every subscriber observes its own clone. Subscribe
    /// before triggering the work whose events are required. A slow subscriber receives an
    /// explicit broadcast lag error and must reconcile against session state. Events written
    /// directly to the primary channel by specialized producers are outside this tap.
    //
    // Used by the next stacked workflow spawn-and-await consumer.
    #[allow(dead_code)]
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.event_observers.subscribe()
    }
}
