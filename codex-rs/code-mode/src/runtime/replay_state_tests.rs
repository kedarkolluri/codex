use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts;
use codex_workflow_journal::AgentStatus;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::ReplayState;

/// A journaled `agent_call` entry at `ordinal` carrying `tokens_spent`, the
/// value the resume loop re-adds to the replay budget accumulator.
fn entry(ordinal: u64, tokens_spent: u64) -> AgentCallLine {
    AgentCallLine {
        timestamp: None,
        ordinal,
        attempt: 0,
        key: format!("blake3:key-{ordinal}"),
        prompt_hash: format!("ph-{ordinal}"),
        opts: AgentCallOpts {
            model: Some("gpt".to_string()),
            effort: Some("high".to_string()),
            agent_type: Some("reviewer".to_string()),
            isolation: None,
            schema_hash: None,
        },
        phase: Some("analyze".to_string()),
        label: Some(format!("file-{ordinal}")),
        child_thread_id: Some(format!("th_{ordinal}")),
        rollout_path: Some(format!("/p/rollout-{ordinal}.jsonl")),
        status: Some(AgentStatus::Completed),
        control_reason: None,
        ret: json!({ "ordinal": ordinal }),
        tokens_spent: Some(tokens_spent),
        progress: None,
        completion_seq: Some(ordinal),
    }
}

#[test]
fn fresh_run_is_inactive_and_empty() {
    // Acceptance: a fresh (non-resume) run initializes with replay inactive
    // and no entries, so live fan-out is never diverted.
    let replay = ReplayState::fresh();
    assert!(!replay.is_active(), "fresh run must start with replay off");
    assert_eq!(replay.prefix_len(), 0);
    assert_eq!(replay.replay_spent(), 0);
    assert!(replay.entry(0).is_none());
    assert!(replay.entry(42).is_none());
}

#[test]
fn seed_indexes_entries_by_ordinal_and_arms_replay() {
    // Acceptance: seeding from a loaded journal populates entries indexed by
    // their invocation ordinal, sets the prefix length, and arms replay.
    // The entries are intentionally passed out of ordinal order to prove the
    // index keys on `ordinal`, not on position.
    let replay = ReplayState::seed(vec![entry(2, 20), entry(0, 5), entry(1, 10)]);
    assert!(replay.is_active(), "a seeded (resumed) run arms replay");
    assert_eq!(replay.prefix_len(), 3, "prefix length M is the entry count");
    assert_eq!(replay.replay_spent(), 0, "accumulator starts at zero");

    for ordinal in 0..3 {
        let found = replay.entry(ordinal).expect("entry present at ordinal");
        assert_eq!(found.ordinal, ordinal, "entry is keyed by its ordinal");
    }
    assert!(
        replay.entry(3).is_none(),
        "ordinals at/after M have no journaled entry"
    );
}

#[test]
fn add_replay_spent_accumulates_while_active() {
    // Acceptance surface for §7 resume step 3: replayed `tokens_spent` is
    // re-added to the replay-only accumulator so the resumed spend curve is
    // byte-identical to the original.
    let mut replay = ReplayState::seed(vec![entry(0, 5), entry(1, 10)]);
    replay.add_replay_spent(5);
    replay.add_replay_spent(10);
    assert_eq!(replay.replay_spent(), 15);
}

#[test]
fn disable_latches_replay_off_permanently() {
    // Acceptance: once `replay_active` is set false it cannot be re-enabled
    // within a run — there is deliberately no re-enable path, and `disable`
    // is idempotent.
    let mut replay = ReplayState::seed(vec![entry(0, 5)]);
    assert!(replay.is_active());

    replay.disable();
    assert!(!replay.is_active(), "first divergence latches replay off");

    // Idempotent: disabling again keeps it off.
    replay.disable();
    assert!(!replay.is_active());

    // The entries remain readable after divergence (they are inert), but
    // replay never re-arms.
    assert!(replay.entry(0).is_some());
    assert!(
        !replay.is_active(),
        "replay stays off; no re-enable path exists"
    );
}

#[test]
fn add_replay_spent_is_frozen_after_divergence() {
    // Divergent (live) calls meter through the real budget, never the replay
    // accumulator — so re-adds are a no-op once replay has been disabled.
    let mut replay = ReplayState::seed(vec![entry(0, 5)]);
    replay.add_replay_spent(5);
    assert_eq!(replay.replay_spent(), 5);

    replay.disable();
    replay.add_replay_spent(100);
    assert_eq!(
        replay.replay_spent(),
        5,
        "no replay budget is accrued after going live"
    );
}

#[test]
fn add_replay_spent_saturates_rather_than_overflows() {
    // A corrupt journal must never panic the run: accumulation saturates.
    let mut replay = ReplayState::seed(vec![entry(0, 1)]);
    replay.add_replay_spent(i64::MAX);
    replay.add_replay_spent(i64::MAX);
    assert_eq!(replay.replay_spent(), i64::MAX);
}
