use super::*;

#[derive(Clone, Copy)]
enum ActiveTurnClaimPhase {
    CurrentTurn,
    Finalizing,
}

impl InputQueue {
    /// Claims pending input for the exact running turn and installs its durable recorder handle.
    #[allow(dead_code)] // Used by the next stacked runtime activation change.
    pub(crate) async fn claim_pending_input_for_turn(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        turn_context: &Arc<TurnContext>,
    ) -> PendingInputClaim {
        self.claim_pending_input_for_active_turn(
            active_turn,
            turn_context,
            ActiveTurnClaimPhase::CurrentTurn,
        )
        .await
    }

    /// Claims only turn-local input while the exact running turn is finalizing.
    #[allow(dead_code)] // Used by the next stacked runtime activation change.
    pub(crate) async fn claim_pending_input_for_finalizing_turn(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        turn_context: &Arc<TurnContext>,
    ) -> PendingInputClaim {
        self.claim_pending_input_for_active_turn(
            active_turn,
            turn_context,
            ActiveTurnClaimPhase::Finalizing,
        )
        .await
    }

    /// Claims only turn-local input from a turn displaced from the active slot.
    ///
    /// The caller must pass the context originally paired with `turn_state`. Once a recording is
    /// installed, subsequent claims authenticate that context by exact shared ownership.
    #[allow(dead_code)] // Used by the next stacked runtime activation change.
    pub(crate) async fn claim_pending_input_for_displaced_turn(
        &self,
        turn_state: &Arc<Mutex<TurnState>>,
        turn_context: &Arc<TurnContext>,
    ) -> PendingInputClaim {
        let mut state = turn_state.lock().await;
        if let Some(recording) = state.pending_input.recording.as_ref() {
            return if recording.matches_turn(turn_context) {
                PendingInputClaim::Recording(recording.clone())
            } else {
                PendingInputClaim::Inactive
            };
        }
        let items = state.pending_input.items.split_off(0);
        Self::install_pending_input_recording(&mut state, turn_state, turn_context, items)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the running-turn check, input claim, and optional mailbox drain must remain atomic"
    )]
    async fn claim_pending_input_for_active_turn(
        &self,
        active_turn: &Mutex<SessionTurnSlot>,
        turn_context: &Arc<TurnContext>,
        phase: ActiveTurnClaimPhase,
    ) -> PendingInputClaim {
        let active = active_turn.lock().await;
        let Some(running_turn) = active
            .running_turn()
            .filter(|running_turn| Arc::ptr_eq(&running_turn.task().turn_context, turn_context))
        else {
            return PendingInputClaim::Inactive;
        };
        let turn_state = Arc::clone(running_turn.turn_state());
        let mut state = turn_state.lock().await;
        if let Some(recording) = state.pending_input.recording.as_ref() {
            return if recording.matches_turn(turn_context) {
                PendingInputClaim::Recording(recording.clone())
            } else {
                PendingInputClaim::Inactive
            };
        }

        let mut mailbox = match phase {
            ActiveTurnClaimPhase::CurrentTurn => {
                if !state.accepts_mailbox_delivery_for_current_turn() {
                    return PendingInputClaim::Empty;
                }
                Some(self.mailbox_pending_mails.lock().await)
            }
            ActiveTurnClaimPhase::Finalizing => None,
        };
        let mut items = state.pending_input.items.split_off(0);
        if let Some(mailbox) = mailbox.as_mut() {
            items.extend(mailbox.drain(..).map(TurnInput::InterAgentCommunication));
        }
        Self::install_pending_input_recording(&mut state, &turn_state, turn_context, items)
    }

    fn install_pending_input_recording(
        state: &mut TurnState,
        turn_state: &Arc<Mutex<TurnState>>,
        turn_context: &Arc<TurnContext>,
        items: Vec<TurnInput>,
    ) -> PendingInputClaim {
        if items.is_empty() {
            return PendingInputClaim::Empty;
        }
        let claimed_input = Arc::<[TurnInput]>::from(items);
        let token = Arc::new(());
        let (result_tx, result_rx) = watch::channel(None);
        let recording = PendingInputRecording {
            token,
            turn_state: Arc::downgrade(turn_state),
            turn_context: Arc::clone(turn_context),
            claimed_input,
            result_rx,
        };
        state.pending_input.recording = Some(recording.clone());
        PendingInputClaim::Acquired(ClaimedPendingInput {
            completion: PendingInputRecordingCompletion {
                recording,
                result_tx: Some(result_tx),
            },
        })
    }
}
