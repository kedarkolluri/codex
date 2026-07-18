use crate::config::RolloutBudgetConfig;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

pub(crate) struct RolloutBudgetReminder {
    pub(crate) remaining_tokens: i64,
    reminder_index: i64,
}

/// Shared accounting and reminder state for one root-thread session tree.
///
/// The inner state lives behind a resettable cell (`Mutex<Option<..>>`) rather
/// than a `OnceLock`, so a reused `AgentControl` — e.g. a nested `workflow()`
/// run — can re-set `limit_tokens` and the weight config without discarding the
/// live, tree-wide `weighted_tokens_used` counter.
#[derive(Default)]
pub(crate) struct RolloutBudget {
    state: Mutex<Option<RolloutBudgetState>>,
}

struct RolloutBudgetState {
    config: RolloutBudgetConfig,
    weighted_tokens_used: f64,
    /// Last reminder delivered to each thread, so every thread observes crossed thresholds.
    deliveries: HashMap<ThreadId, ThreadBudgetDelivery>,
}

struct ThreadBudgetDelivery {
    window_id: String,
    reminder_index: i64,
}

impl RolloutBudget {
    /// Install or re-set the budget configuration.
    ///
    /// The first call initializes the shared state. Subsequent calls update the
    /// `config` (limit, weights, and reminder thresholds) in place while
    /// preserving the live `weighted_tokens_used` counter — so reconfiguring the
    /// ceiling never zeroes accumulated spend.
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
                    deliveries: HashMap::new(),
                });
            }
        }
    }

    /// Current weighted spend read live under the existing lock.
    ///
    /// `pub` because it is the native backing for the JS `budget.spent()` global
    /// (and the pre-admission budget throw), which is wired from outside
    /// `codex-core` in a later Phase 2 ticket. Until that caller lands there is
    /// no in-crate, non-test user, so `dead_code` is still allowed here — the
    /// containing `mod rollout_budget` is private, so a `pub fn` on a
    /// `pub(crate)` type is not yet reachable from the crate's public API.
    #[allow(dead_code)]
    pub fn spent(&self) -> i64 {
        match self.lock().as_ref() {
            Some(state) => state.weighted_tokens_used.floor() as i64,
            None => 0,
        }
    }

    /// Remaining budget: `(limit_tokens - weighted_tokens_used).max(0)`, never negative.
    ///
    /// `pub` because it is the native backing for the JS `budget.remaining()`
    /// global (and the pre-admission budget throw), wired from outside
    /// `codex-core` in a later Phase 2 ticket; `dead_code` stays allowed until
    /// that caller lands (see `spent` above).
    #[allow(dead_code)]
    pub fn remaining(&self) -> i64 {
        match self.lock().as_ref() {
            Some(state) => (state.config.limit_tokens as f64 - state.weighted_tokens_used)
                .max(0.0)
                .floor() as i64,
            None => 0,
        }
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
}
