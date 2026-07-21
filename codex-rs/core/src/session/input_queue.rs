use crate::session::turn_context::TurnContext;
use crate::state::MailboxDeliveryPhase;
use crate::state::SessionTurnSlot;
use crate::state::TurnState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
use tokio::sync::watch;

mod pending_input_claims;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    ResponseItem(ResponseItem),
    InterAgentCommunication(InterAgentCommunication),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    items: Vec<TurnInput>,
    recording: Option<PendingInputRecording>,
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
#[derive(Clone)]
#[must_use = "pending-input recording results must be observed"]
pub(crate) struct PendingInputRecording {
    token: Arc<()>,
    turn_state: Weak<Mutex<TurnState>>,
    turn_context: Arc<TurnContext>,
    claimed_input: Arc<[TurnInput]>,
    result_rx: watch::Receiver<Option<PendingInputRecordingResult>>,
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInputRecordingResult {
    Completed { should_stop: bool },
    Failed,
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
#[must_use = "pending-input claims must be handled"]
pub(crate) enum PendingInputClaim {
    Inactive,
    Empty,
    Recording(PendingInputRecording),
    Acquired(ClaimedPendingInput),
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
#[must_use = "dropping an acquired claim marks its recording failed"]
pub(crate) struct ClaimedPendingInput {
    completion: PendingInputRecordingCompletion,
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
#[must_use = "dropping a completion guard marks its recording failed"]
pub(crate) struct PendingInputRecordingCompletion {
    recording: PendingInputRecording,
    result_tx: Option<watch::Sender<Option<PendingInputRecordingResult>>>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox_pending_mails: Mutex<VecDeque<InterAgentCommunication>>,
}

/// Reversible attachment of explicit and mailbox input to an admitted start.
///
/// Both source and destination remain locked until activation commits. Dropping
/// this guard removes the exact appended suffix and restores drained mailbox
/// messages in FIFO order.
#[must_use = "prepared turn input must be committed or rolled back"]
pub(crate) struct PreparedStartingTurnInput<'a> {
    activity_tx: &'a watch::Sender<InputQueueActivity>,
    turn_state: MutexGuard<'a, TurnState>,
    mailbox: MutexGuard<'a, VecDeque<InterAgentCommunication>>,
    original_pending_len: usize,
    explicit_pending_len: usize,
    committed: bool,
}

impl PreparedStartingTurnInput<'_> {
    #[allow(dead_code)] // Used by the automatic-start activation stages.
    pub(crate) fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.turn_state.pending_input.items[self.original_pending_len + self.explicit_pending_len..]
            .iter()
            .any(|input| {
                matches!(
                    input,
                    TurnInput::InterAgentCommunication(communication) if communication.trigger_turn
                )
            })
    }

    pub(crate) fn commit(mut self) {
        self.committed = true;
    }

    fn rollback(&mut self) {
        let attached = self
            .turn_state
            .pending_input
            .items
            .split_off(self.original_pending_len);
        let mailbox = attached.into_iter().skip(self.explicit_pending_len);
        let mut restored_mail = false;
        for input in mailbox.rev() {
            let TurnInput::InterAgentCommunication(communication) = input else {
                unreachable!("mailbox attachment must append only inter-agent communication");
            };
            self.mailbox.push_front(communication);
            restored_mail = true;
        }
        if restored_mail {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
    }
}

impl Drop for PreparedStartingTurnInput<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.rollback();
        }
    }
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox_pending_mails: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_user_input()
        } else {
            false
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
    ) {
        self.mailbox_pending_mails
            .lock()
            .await
            .push_back(communication);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox_pending_mails.lock().await.is_empty()
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox_pending_mails
            .lock()
            .await
            .iter()
            .any(|mail| mail.trigger_turn)
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> Vec<TurnInput> {
        self.mailbox_pending_mails
            .lock()
            .await
            .drain(..)
            .map(TurnInput::InterAgentCommunication)
            .collect()
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        let running_turn = active.running_turn()?;
        (running_turn.task().turn_context.sub_id == sub_id)
            .then(|| Arc::clone(running_turn.turn_state()))
    }

    /// Clear any pending waiters and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self, turn_state: &Mutex<TurnState>) {
        let mut turn_state = turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.pending_input.items.clear();
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail does not: keep
        // it pending so task completion records it for the next turn without sampling again.
        if turn_state.pending_input.items.iter().any(|input| {
            !matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if !communication.trigger_turn
            )
        }) {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        {
            let mut turn_state = turn_state.lock().await;
            turn_state.pending_input.items.extend(input);
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        turn_state.lock().await.pending_input.items.extend(input);
    }

    /// Prepares explicit and mailbox input for one admitted task start.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "turn-state and mailbox attachment must remain atomic for rollback or commit"
    )]
    pub(crate) async fn prepare_starting_turn_input<'a>(
        &'a self,
        turn_state: &'a Mutex<TurnState>,
        pending_input: Vec<TurnInput>,
    ) -> PreparedStartingTurnInput<'a> {
        let mut turn_state = turn_state.lock().await;
        let mut mailbox = self.mailbox_pending_mails.lock().await;
        let original_pending_len = turn_state.pending_input.items.len();
        let explicit_pending_len = pending_input.len();
        turn_state.pending_input.items.extend(pending_input);
        if turn_state.accepts_mailbox_delivery_for_current_turn() {
            turn_state
                .pending_input
                .items
                .extend(mailbox.drain(..).map(TurnInput::InterAgentCommunication));
        }
        PreparedStartingTurnInput {
            activity_tx: &self.activity_tx,
            turn_state,
            mailbox,
            original_pending_len,
            explicit_pending_len,
            committed: false,
        }
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
    ) -> Vec<TurnInput> {
        let (pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.current_turn_state() {
                Some(turn_state) => {
                    let mut turn_state = turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (pending_input, accepts_mailbox_delivery)
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return pending_input;
        }
        let mailbox_items = self.drain_mailbox_input_items().await.into_iter();
        if pending_input.is_empty() {
            mailbox_items.collect()
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            pending_input
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<SessionTurnSlot>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.current_turn_state() {
                Some(turn_state) => {
                    let turn_state = turn_state.lock().await;
                    (
                        !turn_state.pending_input.items.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if !accepts_mailbox_delivery {
            return false;
        }
        if has_turn_pending_input {
            return true;
        }
        self.has_pending_mailbox_items().await
    }
}

#[cfg(test)]
#[path = "input_queue_tests.rs"]
mod prepared_start_tests;

impl TurnInputQueue {
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
impl ClaimedPendingInput {
    pub(crate) fn recording(&self) -> PendingInputRecording {
        self.completion.recording.clone()
    }

    pub(crate) fn into_parts(self) -> (Arc<[TurnInput]>, PendingInputRecordingCompletion) {
        (self.completion.recording.claimed_input(), self.completion)
    }
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
impl PendingInputRecording {
    fn matches_turn(&self, turn_context: &Arc<TurnContext>) -> bool {
        Arc::ptr_eq(&self.turn_context, turn_context)
    }

    pub(crate) fn claimed_input(&self) -> Arc<[TurnInput]> {
        Arc::clone(&self.claimed_input)
    }

    pub(crate) async fn wait(mut self) -> PendingInputRecordingResult {
        loop {
            let result = {
                let result = self.result_rx.borrow_and_update();
                *result
            };
            if let Some(result) = result {
                return result;
            }
            if self.result_rx.changed().await.is_err() {
                return PendingInputRecordingResult::Failed;
            }
        }
    }

    async fn clear_if_current(&self) {
        let Some(turn_state) = self.turn_state.upgrade() else {
            return;
        };
        let mut state = turn_state.lock().await;
        if state
            .pending_input
            .recording
            .as_ref()
            .is_some_and(|recording| Arc::ptr_eq(&recording.token, &self.token))
        {
            state.pending_input.recording = None;
        }
    }
}

#[allow(dead_code)] // Used by the next stacked runtime activation change.
impl PendingInputRecordingCompletion {
    pub(crate) async fn complete(mut self, should_stop: bool) {
        self.recording.clear_if_current().await;
        if let Some(result_tx) = self.result_tx.take() {
            result_tx.send_replace(Some(PendingInputRecordingResult::Completed { should_stop }));
        }
    }
}

impl Drop for PendingInputRecordingCompletion {
    fn drop(&mut self) {
        if let Some(result_tx) = self.result_tx.take() {
            result_tx.send_replace(Some(PendingInputRecordingResult::Failed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;
    use pretty_assertions::assert_eq;

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "one",
                /*trigger_turn*/ false,
            ))
            .await;
        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "two",
                /*trigger_turn*/ false,
            ))
            .await;

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "queued",
                /*trigger_turn*/ false,
            ))
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        input_queue
            .enqueue_mailbox_communication(make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                "wake",
                /*trigger_turn*/ true,
            ))
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}
