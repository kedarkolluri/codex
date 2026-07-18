use super::*;
use crate::agent::registry::next_thread_spawn_depth;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents::build_agent_spawn_config;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::broadcast::error::TryRecvError;

impl AgentControl {
    /// Spawn a subagent through the **registering** spawn path and block until the child's first
    /// turn completes, returning the child's final assistant message.
    ///
    /// The child is spawned via [`AgentControl::spawn_agent_deferred_input`] →
    /// `spawn_agent_internal` → `spawn_new_thread_with_source(ThreadSource::Subagent)`, the only
    /// path that registers the child in `thread_manager.threads`, fires `notify_thread_created`,
    /// and persists the `agent-graph-store` spawn edge. Those three side effects are exactly what
    /// the workflow monitor, live-attach, and per-agent session-saving features depend on, so this
    /// helper deliberately does NOT use `run_codex_thread_one_shot` / `Codex::spawn`, which skip
    /// all of them.
    ///
    /// The Subagent source is **intrinsic**: the helper takes the parent thread id (non-optional)
    /// and constructs `SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })` itself, so
    /// every caller lands on the registering + spawn-edge-writing path by construction — there is
    /// no `None`-source escape hatch that would silently fall through to `spawn_new_thread` and
    /// skip the edge.
    ///
    /// The child config is built with [`build_agent_spawn_config`] so it inherits the parent
    /// turn's provider/model/reasoning/developer-instructions and runtime state.
    ///
    /// ## Why not drain `next_event()`
    ///
    /// The child's `next_event()` bottoms out at the session's `rx_event`, an `async_channel`
    /// (MPMC work-stealing): each event is delivered to exactly one competing `recv()`. In
    /// production the app-server auto-attaches its own listener to every registered thread
    /// (`notify_thread_created` → `try_attach_thread_listener`), so draining `next_event()` here
    /// would race — the app-server listener could steal `TurnComplete` and this helper would wait
    /// forever. Instead the helper subscribes to the session's **non-competing** broadcast event
    /// tap ([`crate::session::Session::subscribe_events`]), which hands every subscriber a clone.
    /// It subscribes *before* submitting the prompt (the spawn is deferred, so no turn runs until
    /// the helper drives it), guaranteeing the terminal event cannot fire before it is observing.
    ///
    /// Returns:
    /// - `Some(last_agent_message)` on our turn's `EventMsg::TurnComplete`,
    /// - `None` on our turn's `EventMsg::TurnAborted`, a config-build/spawn/submit failure, or the
    ///   child reaching a final state (session-loop termination, or a final status observed after
    ///   the tap lagged) without ever yielding our turn's terminal event.
    // The non-test caller (the cell_actor `SpawnAgent` dispatch) lands in a follow-up ticket;
    // until then this keystone helper is exercised only by its integration tests.
    #[allow(dead_code)]
    pub(crate) async fn spawn_and_await_final_message(
        &self,
        base_instructions: &BaseInstructions,
        parent_turn: &TurnContext,
        parent_thread_id: ThreadId,
        input: Vec<UserInput>,
        mut options: SpawnAgentOptions,
    ) -> Option<String> {
        let config = match build_agent_spawn_config(base_instructions, parent_turn) {
            Ok(config) => config,
            Err(err) => {
                warn!("failed to build subagent spawn config: {err}");
                return None;
            }
        };

        // Make the Subagent source intrinsic: constructing it here (rather than accepting an
        // `Option<SessionSource>`) forces the registering, spawn-edge-writing path for every
        // caller. `spawn_agent_internal` re-derives the agent nickname/path via
        // `prepare_thread_spawn`, so the `None` fields below are placeholders it fills in.
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: next_thread_spawn_depth(&parent_turn.session_source),
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });
        options.parent_thread_id = Some(parent_thread_id);

        let spawned = match self
            .spawn_agent_deferred_input(config, Some(session_source), options)
            .await
        {
            Ok(spawned) => spawned,
            Err(err) => {
                warn!("subagent spawn failed: {err}");
                return None;
            }
        };

        self.await_first_turn_final_message(spawned.thread_id, input)
            .await
    }

    /// Subscribe to the spawned child's non-competing event tap, submit the prompt as a fresh
    /// `Op::UserInput` turn, then drive the tap to **our** turn's terminal event.
    ///
    /// Subscribing before submitting the prompt is load-bearing: a `broadcast::Receiver` only sees
    /// events sent after `subscribe()`, and the deferred spawn means no turn (and therefore no
    /// terminal event) has run yet. Returns the child's final assistant message on `TurnComplete`,
    /// and `None` on `TurnAborted`, a submit failure, or the child reaching a final state without
    /// our turn's terminal event.
    ///
    /// ## Turn identity (defeating foreign steering)
    ///
    /// The child is announced (`notify_thread_created`) *before* this helper submits, so a client
    /// could race a turn onto the child in that window. That is not benign: the public
    /// `Op::UserInput` path ([`AgentControl::send_input`]) does **not** unconditionally start a
    /// fresh turn — if a turn is already active, session dispatch *steers* our prompt into it (see
    /// `handlers::user_input_or_turn_inner` → [`crate::session::Session::steer_input`]) and discards
    /// the steered turn id. The submission id we get back then stamps **no** event: the running
    /// (foreign) turn keeps its own id, its terminal `TurnComplete`/`TurnAborted` carries that
    /// foreign id, our id-filter drops it, and we would hang — or, after a tap lag, recover the
    /// foreign turn's final message from the child's aggregate status.
    ///
    /// A deferred-spawned child runs no turn of its own, so the only way a turn is active here is a
    /// foreign racer. We therefore *refuse to steer*: before submitting we check the child has no
    /// active turn, and after submitting we re-check that the turn our prompt runs under is ours
    /// (defends the narrow enqueue race where a foreign turn goes active between the two steps). If
    /// a foreign turn is active in either check we return `None` — our prompt would be steered and
    /// never run as our own turn, so there is no id to wait on. With steering ruled out, our
    /// submission id is exactly the fresh turn's id, and filtering terminal events on it is sound.
    ///
    /// ## Never waiting forever
    ///
    /// Because this helper holds an `Arc<CodexThread>` for the child, the broadcast tap's sender
    /// stays alive even if the thread is removed from the manager, so `RecvError::Closed` may never
    /// fire. Two deterministic secondary wakes (no wall-clock timeouts) guarantee termination:
    /// - the child's session-loop termination future ([`CodexThread::wait_until_terminated`]): once
    ///   the loop ends, our turn's terminal event can never arrive, so return `None`;
    /// - after a `Lagged` (our terminal event may have been evicted from the ring buffer), the
    ///   child's status watch: once the child reaches a final [`AgentStatus`] we recover its final
    ///   message (or `None`) rather than blocking on an event that will never be re-delivered.
    async fn await_first_turn_final_message(
        &self,
        child_thread_id: ThreadId,
        input: Vec<UserInput>,
    ) -> Option<String> {
        let state = match self.upgrade() {
            Ok(state) => state,
            Err(err) => {
                warn!("thread manager dropped before draining subagent events: {err}");
                return None;
            }
        };
        let child_thread = match state.get_thread(child_thread_id).await {
            Ok(child_thread) => child_thread,
            Err(err) => {
                warn!("spawned subagent {child_thread_id} is not registered: {err}");
                return None;
            }
        };

        // Subscribe BEFORE submitting the prompt so the turn's terminal event cannot be missed.
        // The status watch is the lag/teardown fallback wake (see method docs).
        let mut events = child_thread.subscribe_events();
        let mut status = child_thread.subscribe_status();

        // Refuse to steer into a foreign turn (see "Turn identity" above): a deferred-spawned child
        // runs no turn of its own, so any turn active here is a client that raced into the
        // announce->submit window. Submitting now would let dispatch steer our prompt into that
        // turn, whose terminal event carries the foreign id — never ours — so we could not recover
        // our result and would hang. Bail to `None` instead.
        if let Some(foreign_turn_id) = active_turn_sub_id(&child_thread).await {
            warn!(
                "subagent {child_thread_id} already has active turn {foreign_turn_id} before its \
                 deferred prompt was submitted; refusing to steer the prompt into a foreign turn"
            );
            return None;
        }

        // Deliver the prompt as a fresh `Op::UserInput` turn via the same public `send_input` path
        // a client would use: it re-runs the *current* execution-capacity check at submit time (the
        // deferred spawn's earlier check could be stale — another agent may have taken the last slot
        // in between) and hands back the submission id that stamps our (fresh) turn's events.
        let submission_id = match self.send_input(child_thread_id, input).await {
            Ok(submission_id) => submission_id,
            Err(err) => {
                warn!("failed to submit subagent prompt: {err}");
                return None;
            }
        };

        // Enqueue-race safety net: if a foreign turn went active between the pre-submit check and
        // our submission being processed, our prompt was steered into it and the active turn id is
        // not ours. Our submission id would then stamp no terminal event, so bail rather than wait
        // for one that never arrives. `None` here means the loop has not yet started our fresh turn
        // (its id matches once it does), so it is not a false positive.
        if let Some(active_turn_id) = active_turn_sub_id(&child_thread).await
            && active_turn_id != submission_id
        {
            warn!(
                "subagent {child_thread_id} prompt was steered into foreign turn {active_turn_id} \
                 (expected fresh turn {submission_id}); reporting None"
            );
            return None;
        }

        // Set once the tap lags past its ring buffer: our terminal event may have been evicted, so
        // from then on a final status transition is treated as the authoritative turn outcome.
        let mut tap_lagged = false;
        loop {
            tokio::select! {
                // `biased`: poll the event tap first so a terminal event for our turn that is
                // already buffered/ready always wins over the teardown and status wakes below. An
                // unbiased select could otherwise pick the teardown arm even when our `TurnComplete`
                // is ready, dropping the final message and returning `None`.
                biased;

                received = events.recv() => match received {
                    Ok(event) => {
                        // Only our (fresh) turn's terminal events are authoritative. Steering is
                        // ruled out above, so a matching id can only be our own turn; foreign turns
                        // carry a different id and are ignored.
                        if event.id != submission_id {
                            continue;
                        }
                        match event.msg {
                            EventMsg::TurnComplete(turn_complete) => {
                                return turn_complete.last_agent_message;
                            }
                            EventMsg::TurnAborted(_) => return None,
                            _ => {}
                        }
                    }
                    // The tap lagged past the ring buffer; our terminal event may have been evicted.
                    // Reconcile against the child's status: if it already reached a final state,
                    // recover its message (or `None`) instead of scanning for an event that will
                    // never be re-delivered. If not yet final, keep scanning — the now-armed status
                    // arm below will wake us when the turn ends.
                    Err(RecvError::Lagged(_)) => {
                        tap_lagged = true;
                        if let Some(recovered) =
                            final_turn_message_from_status(&status.borrow_and_update())
                        {
                            return recovered;
                        }
                    }
                    // Tap closed (session dropped) without a terminal event: treat as interrupted.
                    Err(RecvError::Closed) => return None,
                },
                // Deterministic teardown wake: the child's session loop ended. Our turn's terminal
                // event can no longer *arrive* on the tap (yet we still hold the child `Arc`, so
                // `RecvError::Closed` never fires). Before giving up, drain any terminal event for
                // our turn already buffered on the tap — a teardown that races our own
                // `TurnComplete` must not drop the final message.
                () = child_thread.wait_until_terminated() => {
                    return drain_buffered_final_message(&mut events, &submission_id);
                }
                // After a lag we can no longer rely on the tap for our specific terminal event, so
                // also wake on status transitions and recover the child's final outcome. Disabled
                // until a lag actually happens so a foreign turn's completion can never make us
                // return prematurely on the normal (non-lagged) path.
                changed = status.changed(), if tap_lagged => {
                    if changed.is_err() {
                        // Status sender dropped: the child is gone.
                        return None;
                    }
                    if let Some(recovered) =
                        final_turn_message_from_status(&status.borrow_and_update())
                    {
                        return recovered;
                    }
                }
            }
        }
    }
}

/// Read the sub_id of the child's currently-active turn, if any.
///
/// A deferred-spawned child runs no turn of its own until the caller drives the first turn, so a
/// non-`None` result before/just-after the caller submits its prompt is a client that raced a turn
/// onto the child in the announce->submit window (see [`AgentControl::await_first_turn_final_message`]
/// "Turn identity").
async fn active_turn_sub_id(child_thread: &Arc<crate::CodexThread>) -> Option<String> {
    child_thread
        .codex
        .session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|turn| turn.task.as_ref())
        .map(|task| task.turn_context.sub_id.clone())
}

/// Non-blocking drain of already-buffered tap events, returning our turn's terminal outcome if its
/// `TurnComplete`/`TurnAborted` is already sitting in the broadcast ring buffer.
///
/// Used on the teardown wake so a terminal event that races session-loop termination is recovered
/// rather than dropped. Returns `None` once the buffer holds no matching terminal event
/// (empty/closed) — i.e. the child really did tear down without finishing our turn.
fn drain_buffered_final_message(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    submission_id: &str,
) -> Option<String> {
    loop {
        match events.try_recv() {
            Ok(event) => {
                if event.id != *submission_id {
                    continue;
                }
                match event.msg {
                    EventMsg::TurnComplete(turn_complete) => {
                        return turn_complete.last_agent_message;
                    }
                    EventMsg::TurnAborted(_) => return None,
                    _ => {}
                }
            }
            // Skip the lag marker and keep draining the events still buffered behind it.
            Err(TryRecvError::Lagged(_)) => continue,
            Err(TryRecvError::Empty | TryRecvError::Closed) => return None,
        }
    }
}

/// Map a child's [`AgentStatus`] to a spawn-and-await outcome, used only as the lag/teardown
/// fallback when the event tap can no longer deliver our turn's terminal event.
///
/// Returns:
/// - `Some(message)` once the child reached a terminal state (`Completed` recovers its final
///   message; `Errored`/`Shutdown`/`NotFound`/`Interrupted` resolve to `None`),
/// - `None` while the child is still `PendingInit`/`Running` (keep waiting).
fn final_turn_message_from_status(status: &AgentStatus) -> Option<Option<String>> {
    match status {
        AgentStatus::Completed(message) => Some(message.clone()),
        AgentStatus::Errored(_)
        | AgentStatus::Shutdown
        | AgentStatus::NotFound
        | AgentStatus::Interrupted => Some(None),
        AgentStatus::PendingInit | AgentStatus::Running => None,
    }
}

#[cfg(test)]
#[path = "spawn_await_tests.rs"]
mod tests;
