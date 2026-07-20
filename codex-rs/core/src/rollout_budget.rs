use crate::config::RolloutBudgetConfig;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

pub(crate) struct RolloutBudgetReminder {
    pub(crate) remaining_tokens: i64,
    reminder_index: i64,
}

/// Outcome of an atomic pre-spawn budget [`RolloutBudget::reserve`].
///
/// Reservation is the race-free session-ceiling gate for a workflow `agent()` fan-out:
/// N concurrent `parallel()`/`pipeline()` admissions all serialize on the single
/// state mutex, so they can no longer each observe headroom via an unreserved
/// `remaining()` read before any child records usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetAdmission {
    /// No session budget configured, so the spawn is admitted without recording
    /// a reservation.
    Unmetered,
    /// Admitted: `estimate` weighted tokens were reserved against the ceiling and
    /// must be released with [`RolloutBudget::release_reservation`] on finalize.
    Reserved,
    /// Rejected: recorded spend plus outstanding reservations have already reached
    /// `limit_tokens`, so the ceiling is (about to be) crossed.
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RolloutBudgetExceeded;

/// Cancellation-safe ownership of one session-budget admission reservation.
pub(crate) struct RolloutBudgetReservation {
    budget: Arc<RolloutBudget>,
    estimate: i64,
    reserved: bool,
}

impl Drop for RolloutBudgetReservation {
    fn drop(&mut self) {
        if self.reserved {
            self.budget.release_reservation(self.estimate);
        }
    }
}

/// Shared accounting and reminder state for one root-thread session tree.
///
/// This is exclusively the session ceiling. Workflow runs use independent
/// `core_workflows::WorkflowBudget` meters and never mutate this configuration.
#[derive(Default)]
pub(crate) struct RolloutBudget {
    state: Mutex<Option<RolloutBudgetState>>,
}

struct RolloutBudgetState {
    config: RolloutBudgetConfig,
    weighted_tokens_used: f64,
    /// Weighted tokens reserved by in-flight `agent()` admissions that have not
    /// yet recorded their actual usage (see [`RolloutBudget::reserve`]). Counted
    /// against the ceiling at admission time so concurrent admissions serialize;
    /// released on finalize once the child's real usage lands in
    /// `weighted_tokens_used`.
    reserved: f64,
    /// Last reminder delivered to each thread, so every thread observes crossed thresholds.
    deliveries: HashMap<ThreadId, ThreadBudgetDelivery>,
}

struct ThreadBudgetDelivery {
    window_id: String,
    reminder_index: i64,
}

impl RolloutBudget {
    /// Install the session budget configuration.
    ///
    /// The production call site is session `AgentControl` construction. If the
    /// same control is configured again, accounting is preserved.
    ///
    /// When the new config actually differs from the installed one, the
    /// per-thread reminder-delivery bookkeeping is reset: the old deliveries were
    /// recorded against the previous thresholds/limit, so keeping them could
    /// suppress the first reminder of the new configuration. An identical config
    /// leaves everything (spend and deliveries) untouched.
    pub(crate) fn configure(&self, config: RolloutBudgetConfig) {
        let mut guard = self.lock();
        match guard.as_mut() {
            Some(state) => {
                if state.config != config {
                    state.config = config;
                    state.deliveries.clear();
                }
            }
            None => {
                *guard = Some(RolloutBudgetState {
                    config,
                    weighted_tokens_used: 0.0,
                    reserved: 0.0,
                    deliveries: HashMap::new(),
                });
            }
        }
    }

    /// The configured ceiling (`limit_tokens`), or `None` when unmetered.
    ///
    /// This is the authoritative "is this run metered?" signal: it distinguishes an
    /// explicit zero ceiling (`Some(0)`, which rejects the first `agent()`) from an
    /// unconfigured budget (`None`, never gated) — a distinction the
    /// `spent() + remaining()` heuristic cannot make, because a zero-limit config
    /// also reads 0 from both getters.
    pub(crate) fn limit(&self) -> Option<i64> {
        self.lock().as_ref().map(|state| state.config.limit_tokens)
    }

    /// Atomically reserve `estimate` weighted tokens against the ceiling before a
    /// workflow `agent()` child is spawned (spec §5 admission, §8).
    ///
    /// Concurrent admissions serialize on the single state mutex: a spawn is
    /// admitted ([`BudgetAdmission::Reserved`]) only while recorded spend plus
    /// outstanding reservations are still below `limit_tokens`; once they reach the
    /// ceiling every further admission is [`BudgetAdmission::Rejected`]. An
    /// unconfigured budget is [`BudgetAdmission::Unmetered`] (never gated, no
    /// reservation recorded).
    ///
    /// ## Overshoot bound
    ///
    /// The caller reserves at the top of the concurrency-permit region and releases
    /// on finalize, so a reservation exists only for a child that currently holds
    /// one of the run's `C` concurrency slots. At most `C` reservations therefore
    /// exist at once, and a new admission is refused the moment
    /// `weighted_tokens_used + reserved >= limit_tokens`. The ceiling thus overshoots
    /// by at most one in-flight turn per concurrency slot — never by "up to N" for an
    /// N-wide `parallel()`/`pipeline()` fan-out, which is the defect the previous
    /// unreserved `remaining()` read allowed. `estimate` is a conservative per-turn
    /// floor; because reservations are scoped to permit holders, the `C`-slot bound
    /// holds for any positive estimate.
    pub(crate) fn reserve(&self, estimate: i64) -> BudgetAdmission {
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return BudgetAdmission::Unmetered;
        };
        if state.weighted_tokens_used + state.reserved >= state.config.limit_tokens as f64 {
            return BudgetAdmission::Rejected;
        }
        state.reserved += estimate.max(0) as f64;
        BudgetAdmission::Reserved
    }

    /// Reserve against the session ceiling with an owned RAII guard.
    ///
    /// Workflow callbacks hold this guard across the child spawn. Dropping the
    /// callback future (including cancellation) releases the estimate, while the
    /// session's normal token accounting remains responsible for actual usage.
    pub(crate) fn reserve_owned(
        self: &Arc<Self>,
        estimate: i64,
    ) -> Result<RolloutBudgetReservation, RolloutBudgetExceeded> {
        let admission = self.reserve(estimate);
        if matches!(admission, BudgetAdmission::Rejected) {
            return Err(RolloutBudgetExceeded);
        }
        Ok(RolloutBudgetReservation {
            budget: Arc::clone(self),
            estimate,
            reserved: matches!(admission, BudgetAdmission::Reserved),
        })
    }

    /// Release a reservation taken by [`reserve`](Self::reserve) on finalize (the
    /// success AND the failure/abort path alike). The child's real usage has by then
    /// been recorded via [`record_usage`](Self::record_usage), so dropping the
    /// estimate reconciles the reservation against actual spend. Clamped at 0 so a
    /// an over-release can never drive `reserved` negative.
    pub(crate) fn release_reservation(&self, estimate: i64) {
        let mut guard = self.lock();
        if let Some(state) = guard.as_mut() {
            state.reserved = (state.reserved - estimate.max(0) as f64).max(0.0);
        }
    }

    #[cfg(test)]
    fn spent(&self) -> i64 {
        match self.lock().as_ref() {
            Some(state) => state.weighted_tokens_used.floor() as i64,
            None => 0,
        }
    }

    /// Remaining budget: `(limit_tokens - weighted_tokens_used).max(0)`, never negative.
    pub(crate) fn remaining(&self) -> i64 {
        match self.lock().as_ref() {
            Some(state) => (state.config.limit_tokens as f64 - state.weighted_tokens_used)
                .max(0.0)
                .floor() as i64,
            None => 0,
        }
    }

    /// The AUTHORITATIVE pre-admission budget gate predicate, shared verbatim by
    /// the production `CoreTurnHost::spawn_agent` cheap pre-check and the unit
    /// tests (no test-only copy).
    ///
    /// A METERED run (`limit().is_some()`, including an explicit `Some(0)`) whose
    /// `remaining()` has hit `<= 0` refuses the next `agent()`; an UNMETERED run
    /// (`None`) is never gated, so an ordinary un-budgeted workflow keeps spawning.
    /// `limit()` — not the old `spent() + remaining() > 0` heuristic — is the
    /// authoritative metered signal, so a real zero ceiling is distinguished from an
    /// unconfigured budget (both read 0 from the getters).
    pub(crate) fn pre_admission_rejects(&self) -> bool {
        self.limit().is_some() && self.remaining() <= 0
    }

    /// Returns true once the configured budget is exhausted, including on later calls.
    pub(crate) fn record_usage(&self, usage: &TokenUsage) -> bool {
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return false;
        };
        state.weighted_tokens_used += usage.output_tokens.max(0) as f64
            * state.config.sampling_token_weight
            + usage.non_cached_input() as f64 * state.config.prefill_token_weight;
        state.weighted_tokens_used >= state.config.limit_tokens as f64
    }

    pub(crate) fn pending_reminder(
        &self,
        thread_id: ThreadId,
        window_id: &str,
    ) -> Option<RolloutBudgetReminder> {
        let guard = self.lock();
        let state = guard.as_ref()?;
        // An empty threshold list means "inject no budget reminders at all" — not
        // even the initial remaining-token restatement. Without this guard the
        // first call still surfaces reminder index 0 with the current remainder.
        if state.config.reminder_at_remaining_tokens.is_empty() {
            return None;
        }
        let remaining_tokens = (state.config.limit_tokens as f64 - state.weighted_tokens_used)
            .max(0.0)
            .floor() as i64;
        let reminder_index = state
            .config
            .reminder_at_remaining_tokens
            .iter()
            .filter(|&&threshold| remaining_tokens <= threshold)
            .count() as i64;
        if state.deliveries.get(&thread_id).is_some_and(|delivery| {
            delivery.window_id.as_str() == window_id && delivery.reminder_index >= reminder_index
        }) {
            return None;
        }
        Some(RolloutBudgetReminder {
            remaining_tokens,
            reminder_index,
        })
    }

    pub(crate) fn mark_reminder_delivered(
        &self,
        thread_id: ThreadId,
        window_id: &str,
        reminder: RolloutBudgetReminder,
    ) {
        // Mark delivery only after history insertion; cancellation before then should retry it.
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        state.deliveries.insert(
            thread_id,
            ThreadBudgetDelivery {
                window_id: window_id.to_string(),
                reminder_index: reminder.reminder_index,
            },
        );
    }

    /// Forces the next sampling request for `thread_id` to restate the current remainder.
    pub(crate) fn rearm_reminder(&self, thread_id: ThreadId) {
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        state.deliveries.remove(&thread_id);
    }

    fn lock(&self) -> MutexGuard<'_, Option<RolloutBudgetState>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn config(limit_tokens: i64) -> RolloutBudgetConfig {
        RolloutBudgetConfig {
            limit_tokens,
            reminder_at_remaining_tokens: Vec::new(),
            sampling_token_weight: 1.0,
            prefill_token_weight: 0.0,
        }
    }

    fn config_with_reminders(limit_tokens: i64, thresholds: Vec<i64>) -> RolloutBudgetConfig {
        RolloutBudgetConfig {
            limit_tokens,
            reminder_at_remaining_tokens: thresholds,
            sampling_token_weight: 1.0,
            prefill_token_weight: 0.0,
        }
    }

    fn output_usage(output_tokens: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens,
            reasoning_output_tokens: 0,
            total_tokens: output_tokens,
        }
    }

    /// The pre-admission budget gate asserted by these tests is the SAME predicate the production
    /// `CoreTurnHost::spawn_agent` cheap pre-check calls — [`RolloutBudget::pre_admission_rejects`] —
    /// not a test-only copy. This thin wrapper keeps the call sites in the existing tests readable
    /// while guaranteeing they exercise the real product code (a change to the ceiling contract
    /// updates both the host and these assertions in lockstep).
    fn would_throw_pre_admission(budget: &RolloutBudget) -> bool {
        budget.pre_admission_rejects()
    }

    #[test]
    fn getters_default_to_zero_when_unconfigured() {
        let budget = RolloutBudget::default();
        assert_eq!(budget.spent(), 0);
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn remaining_is_never_negative() {
        let budget = RolloutBudget::default();
        budget.configure(config(100));
        budget.record_usage(&output_usage(150));
        assert_eq!(budget.spent(), 150);
        assert_eq!(budget.remaining(), 0);
    }

    /// The pre-admission gate fires exactly when `remaining()` first hits `<= 0`, and not before:
    /// every turn up to (and including) the one that lands spend exactly on the limit is admitted,
    /// then the next admission is refused. This is the deterministic ceiling the UAT-5 fixture rides.
    #[test]
    fn pre_admission_gate_fires_exactly_at_remaining_zero() {
        let budget = RolloutBudget::default();
        budget.configure(config(250));
        // Fresh metered run: remaining 250 > 0, admit.
        assert!(
            !would_throw_pre_admission(&budget),
            "a fresh metered run must admit"
        );
        budget.record_usage(&output_usage(100)); // remaining 150
        assert_eq!(budget.remaining(), 150);
        assert!(!would_throw_pre_admission(&budget));
        budget.record_usage(&output_usage(100)); // remaining 50
        assert_eq!(budget.remaining(), 50);
        assert!(!would_throw_pre_admission(&budget));
        // The turn that lands spend exactly on the limit leaves remaining() == 0: the gate closes
        // for the NEXT admission, not this one.
        budget.record_usage(&output_usage(50)); // spent 250, remaining 0
        assert_eq!(budget.spent(), 250);
        assert_eq!(budget.remaining(), 0);
        assert!(
            would_throw_pre_admission(&budget),
            "the gate must fire exactly at remaining() == 0"
        );
    }

    /// An UNMETERED run (unconfigured budget) reads 0 from both getters; that 0 must NOT be treated
    /// as an exhausted ceiling, or every `agent()` in an un-budgeted workflow would throw.
    #[test]
    fn unmetered_run_is_never_gated_pre_admission() {
        let budget = RolloutBudget::default();
        assert_eq!(budget.spent(), 0);
        assert_eq!(budget.remaining(), 0);
        assert!(
            !would_throw_pre_admission(&budget),
            "an unmetered run must never be gated by the budget ceiling"
        );
    }

    /// The ceiling overshoots by at most one in-flight turn: a turn is admitted only while
    /// `remaining() > 0`, so the single turn that crosses the limit is the only overshoot — final
    /// spend never exceeds the ceiling by more than one turn's usage. This is the §8 one-turn
    /// overshoot bound backing the in-flight backstop.
    #[test]
    fn ceiling_overshoots_by_at_most_one_in_flight_turn() {
        const LIMIT: i64 = 250;
        const TURN: i64 = 100;
        let budget = RolloutBudget::default();
        budget.configure(config(LIMIT));

        // Admit turns until the pre-admission gate closes, exactly as the host does.
        let mut admitted = 0;
        while !would_throw_pre_admission(&budget) {
            budget.record_usage(&output_usage(TURN));
            admitted += 1;
            assert!(
                admitted < 1_000,
                "the admission loop must terminate at the ceiling"
            );
        }

        // The crossing turn pushed spend to/over the limit, but by less than one full turn.
        let overshoot = budget.spent() - LIMIT;
        assert!(
            overshoot >= 0,
            "the crossing turn must have reached the limit (spent {}, limit {LIMIT})",
            budget.spent()
        );
        assert!(
            overshoot < TURN,
            "the ceiling must overshoot by at most one in-flight turn (overshoot {overshoot} < {TURN})"
        );
        // Once closed the gate stays closed and remaining() is clamped at 0.
        assert_eq!(budget.remaining(), 0);
        assert!(would_throw_pre_admission(&budget));
    }

    #[test]
    fn spent_reflects_tree_wide_sum_across_cloned_handles() {
        let budget = Arc::new(RolloutBudget::default());
        budget.configure(config(1_000));

        // Simulate the root thread and cloned sub-agent control handles sharing
        // the same Arc and each recording their own turn usage.
        let root = Arc::clone(&budget);
        let child_a = Arc::clone(&budget);
        let child_b = Arc::clone(&budget);

        root.record_usage(&output_usage(100));
        child_a.record_usage(&output_usage(30));
        child_b.record_usage(&output_usage(70));

        assert_eq!(budget.spent(), 200);
        assert_eq!(budget.remaining(), 800);
    }

    #[test]
    fn only_output_tokens_are_counted_with_output_weight_config() {
        let budget = RolloutBudget::default();
        budget.configure(config(1_000));
        budget.record_usage(&TokenUsage {
            input_tokens: 500,
            cached_input_tokens: 0,
            output_tokens: 40,
            reasoning_output_tokens: 0,
            total_tokens: 540,
        });
        // prefill_token_weight is 0.0, so the 500 input tokens are ignored.
        assert_eq!(budget.spent(), 40);
        assert_eq!(budget.remaining(), 960);
    }

    #[test]
    fn reconfigure_updates_limit_without_resetting_spend() {
        let budget = RolloutBudget::default();
        // Configure with limit A, then spend.
        budget.configure(config(1_000));
        budget.record_usage(&output_usage(400));
        assert_eq!(budget.spent(), 400);
        assert_eq!(budget.remaining(), 600);

        // Reconfigure with limit B: the new ceiling takes effect and the live
        // spend is preserved (not zeroed).
        budget.configure(config(500));
        assert_eq!(budget.spent(), 400);
        assert_eq!(budget.remaining(), 100);
    }

    #[test]
    fn reconfigure_updates_weights() {
        let budget = RolloutBudget::default();
        budget.configure(config(1_000));
        budget.record_usage(&output_usage(100));
        assert_eq!(budget.spent(), 100);

        // Swap to a config that also counts prefill tokens.
        budget.configure(RolloutBudgetConfig {
            limit_tokens: 1_000,
            reminder_at_remaining_tokens: Vec::new(),
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
        });
        budget.record_usage(&TokenUsage {
            input_tokens: 50,
            cached_input_tokens: 0,
            output_tokens: 10,
            reasoning_output_tokens: 0,
            total_tokens: 60,
        });
        // 100 (preserved) + 10 output + 50 non-cached input.
        assert_eq!(budget.spent(), 160);
    }

    #[test]
    fn reconfigure_with_changed_config_resets_reminder_deliveries() {
        let budget = RolloutBudget::default();
        let thread = ThreadId::new();
        let window = "w1";

        // Configure with a reminder threshold, cross it, and deliver the reminder.
        budget.configure(config_with_reminders(1_000, vec![500]));
        budget.record_usage(&output_usage(600));
        let reminder = budget
            .pending_reminder(thread, window)
            .expect("threshold crossed, reminder should be pending");
        assert_eq!(reminder.remaining_tokens, 400);
        assert_eq!(reminder.reminder_index, 1);
        budget.mark_reminder_delivered(thread, window, reminder);
        // Already delivered under the current config, so nothing pending now.
        assert!(budget.pending_reminder(thread, window).is_none());

        // Reconfigure with a genuinely different config (tighter limit). The
        // accumulated spend must survive, but the stale delivery must not
        // suppress the first reminder of the new configuration.
        budget.configure(config_with_reminders(800, vec![500]));
        assert_eq!(budget.spent(), 600);
        let reminder = budget
            .pending_reminder(thread, window)
            .expect("reconfigure must re-arm the reminder for the new config");
        // remaining = 800 - 600 = 200, still under the 500 threshold.
        assert_eq!(reminder.remaining_tokens, 200);
        assert_eq!(reminder.reminder_index, 1);
    }

    #[test]
    fn reconfigure_with_identical_config_preserves_reminder_deliveries() {
        let budget = RolloutBudget::default();
        let thread = ThreadId::new();
        let window = "w1";

        budget.configure(config_with_reminders(1_000, vec![500]));
        budget.record_usage(&output_usage(600));
        let reminder = budget
            .pending_reminder(thread, window)
            .expect("threshold crossed, reminder should be pending");
        budget.mark_reminder_delivered(thread, window, reminder);
        assert!(budget.pending_reminder(thread, window).is_none());

        // Reconfiguring with an identical config must be a no-op: the delivery
        // bookkeeping (and spend) is untouched, so nothing becomes pending.
        budget.configure(config_with_reminders(1_000, vec![500]));
        assert_eq!(budget.spent(), 600);
        assert!(
            budget.pending_reminder(thread, window).is_none(),
            "identical reconfigure must not re-arm an already-delivered reminder"
        );
    }

    /// `limit()` distinguishes an explicit zero ceiling from an unconfigured budget:
    /// `None` unconfigured, `Some(0)` for a real zero ceiling, `Some(n)` otherwise.
    #[test]
    fn limit_distinguishes_unmetered_from_zero_ceiling() {
        let budget = RolloutBudget::default();
        assert_eq!(budget.limit(), None, "unconfigured budget is unmetered");
        budget.configure(config(0));
        assert_eq!(
            budget.limit(),
            Some(0),
            "an explicit zero ceiling is metered"
        );
        assert!(
            would_throw_pre_admission(&budget),
            "a zero ceiling must reject the first agent()"
        );
        budget.configure(config(100));
        assert_eq!(budget.limit(), Some(100));
    }

    /// An unmetered budget admits every reservation without recording one; a metered
    /// budget admits while there is headroom and rejects once spend plus outstanding
    /// reservations reach the ceiling — the race-free serialization the fan-out needs.
    #[test]
    fn reserve_serializes_admissions_against_the_ceiling() {
        // Unmetered: always admitted, nothing reserved.
        let budget = RolloutBudget::default();
        assert_eq!(budget.reserve(1_000), BudgetAdmission::Unmetered);
        assert_eq!(budget.spent(), 0);

        // Metered with a 250 ceiling and a 100-token per-turn estimate.
        let budget = RolloutBudget::default();
        budget.configure(config(250));
        assert_eq!(budget.reserve(100), BudgetAdmission::Reserved); // reserved 100
        assert_eq!(budget.reserve(100), BudgetAdmission::Reserved); // reserved 200
        // spent 0 + reserved 200 < 250, still room for one more.
        assert_eq!(budget.reserve(100), BudgetAdmission::Reserved); // reserved 300
        // spent 0 + reserved 300 >= 250: the ceiling is (about to be) crossed.
        assert_eq!(budget.reserve(100), BudgetAdmission::Rejected);

        // Releasing a reservation reopens headroom.
        budget.release_reservation(100); // reserved back to 200
        assert_eq!(budget.reserve(100), BudgetAdmission::Reserved);
    }

    #[tokio::test]
    async fn cancelling_an_owned_reservation_restores_session_headroom() {
        let budget = Arc::new(RolloutBudget::default());
        budget.configure(config(100));
        let task_budget = Arc::clone(&budget);
        let task = tokio::spawn(async move {
            let _reservation = task_budget
                .reserve_owned(100)
                .expect("first admission should reserve");
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert!(budget.reserve_owned(1).is_err());

        task.abort();
        let _ = task.await;

        assert!(budget.reserve_owned(100).is_ok());
    }

    /// Concurrent admissions can NOT all pass the ceiling: N threads racing
    /// [`reserve`](RolloutBudget::reserve) against a shared budget with no
    /// intervening release serialize on the state mutex, so only the reservations
    /// that fit under `limit_tokens` are `Reserved` and every further one is
    /// `Rejected` — the race-free bound that closes the pre-fix hole where an N-wide
    /// `parallel()` fan-out could each read headroom before any child recorded usage
    /// and admit all N. With `limit = 250` and a `100`-token estimate, exactly
    /// `ceil` = 3 reservations fit (0+100, 100+100, 200+100 all `< 250`; 300 `>=`
    /// 250 rejects), regardless of how many threads race.
    #[test]
    fn concurrent_reserve_admits_at_most_the_ceiling_bound() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        const LIMIT: i64 = 250;
        const ESTIMATE: i64 = 100;
        const RACERS: usize = 16;
        // ceil headroom: reservations fit while used+reserved < LIMIT.
        const EXPECTED_RESERVED: usize = 3;

        let budget = Arc::new(RolloutBudget::default());
        budget.configure(config(LIMIT));

        // A barrier so every thread hits `reserve` in the same window — the reservation, not thread
        // start-up staggering, is what must bound the admissions.
        let barrier = Arc::new(Barrier::new(RACERS));
        let reserved = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..RACERS {
            let budget = Arc::clone(&budget);
            let barrier = Arc::clone(&barrier);
            let reserved = Arc::clone(&reserved);
            let rejected = Arc::clone(&rejected);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                match budget.reserve(ESTIMATE) {
                    BudgetAdmission::Reserved => reserved.fetch_add(1, Ordering::SeqCst),
                    BudgetAdmission::Rejected => rejected.fetch_add(1, Ordering::SeqCst),
                    BudgetAdmission::Unmetered => {
                        panic!("a configured budget must never report Unmetered")
                    }
                };
            }));
        }
        for handle in handles {
            handle.join().expect("reserve racer thread panicked");
        }

        assert_eq!(
            reserved.load(Ordering::SeqCst),
            EXPECTED_RESERVED,
            "exactly the reservations that fit under the ceiling may be admitted, no matter how \
             many threads race"
        );
        assert_eq!(
            rejected.load(Ordering::SeqCst),
            RACERS - EXPECTED_RESERVED,
            "every admission over the ceiling bound is rejected"
        );
        // The reserved total never exceeds the ceiling: no fan-out can blow past it.
        assert!(
            EXPECTED_RESERVED as i64 * ESTIMATE <= LIMIT + ESTIMATE,
            "reserved tokens stay within the one-estimate overshoot bound"
        );
    }

    /// A zero ceiling rejects the very first reservation (spec §3 zero-budget).
    #[test]
    fn reserve_rejects_first_admission_at_zero_ceiling() {
        let budget = RolloutBudget::default();
        budget.configure(config(0));
        assert_eq!(budget.reserve(1), BudgetAdmission::Rejected);
    }

    /// `release_reservation` is clamped at 0 so a reset (or an over-release) can never
    /// drive `reserved` negative and wrongly reopen budget headroom.
    #[test]
    fn release_reservation_is_clamped_at_zero() {
        let budget = RolloutBudget::default();
        budget.configure(config(100));
        budget.release_reservation(1_000); // no reservation outstanding
        // Still admits normally (reserved stayed at 0, not negative).
        assert_eq!(budget.reserve(50), BudgetAdmission::Reserved);
    }

    /// A recorded turn frees the reserved estimate's headroom once released: the
    /// reservation is an admission placeholder, actual spend is what accrues.
    #[test]
    fn reservation_reconciles_against_recorded_usage() {
        let budget = RolloutBudget::default();
        budget.configure(config(1_000));
        assert_eq!(budget.reserve(100), BudgetAdmission::Reserved);
        // The child records its real usage, then finalize releases the estimate.
        budget.record_usage(&output_usage(80));
        budget.release_reservation(100);
        // Only the actual 80 tokens accrued; the estimate left no residue.
        assert_eq!(budget.spent(), 80);
        assert_eq!(budget.remaining(), 920);
    }

    /// Finding #10: an empty `reminder_at_remaining_tokens` suppresses ALL reminders,
    /// including the initial remaining-token restatement, so no schedule-dependent
    /// budget text is injected into model context.
    #[test]
    fn empty_reminder_thresholds_suppress_all_reminders() {
        let budget = RolloutBudget::default();
        let thread = ThreadId::new();
        budget.configure(config(1_000));
        assert!(
            budget.pending_reminder(thread, "w1").is_none(),
            "empty thresholds must suppress the initial reminder"
        );
        budget.record_usage(&output_usage(900));
        assert!(
            budget.pending_reminder(thread, "w1").is_none(),
            "empty thresholds must suppress reminders even near exhaustion"
        );
    }
}
