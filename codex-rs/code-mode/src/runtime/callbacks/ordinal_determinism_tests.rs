//! Property/fuzz coverage for the §7 invocation-ordinal spine (`P3-uat-resume-gate`
//! acceptance: "property/fuzz test over random parallel/pipeline shapes confirms
//! ordinal determinism"). The cache spine keys on the SOURCE-ORDER invocation
//! ordinal stamped synchronously in [`agent_callback`], never on completion order.
//! These drive fresh (non-resume) fan-outs of random width through the real V8
//! runtime, resolve them in a random (out-of-order) completion order, and assert
//! the emitted `AgentCall` ordinals are STILL the strict source-order sequence
//! `0..N` — the invariant that makes prefix-replay cache keys reproducible run to
//! run regardless of which subagent settles first. Randomness is a deterministic
//! splitmix64 PRNG (no wall-clock, no `std` rng), honouring the §7 contract.
use std::collections::HashMap;
use std::time::Duration;

use codex_code_mode_protocol::ExecuteRequest;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;

use super::super::PendingRuntimeMode;
use super::super::RuntimeCommand;
use super::super::RuntimeEvent;
use super::super::spawn_runtime;
use crate::FunctionCallOutputContentItem;

fn workflow_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        workflow: true,
        args: None,
        run_id: None,
        replay_entries: Vec::new(),
        workflow_budget: None,
    }
}

/// A deterministic splitmix64 step — the only entropy source these tests use.
fn next_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fisher-Yates a `0..len` permutation with the deterministic PRNG.
fn permutation(len: usize, state: &mut u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    for i in (1..len).rev() {
        let j = (next_rand(state) as usize) % (i + 1);
        order.swap(i, j);
    }
    order
}

/// Drive a fresh `parallel()` fan-out of `width` agents (prompts `p0..p{width-1}`),
/// buffering every `AgentCall` until all `width` have dispatched, then resolving them
/// in `response_order` (a permutation of `0..width`) — an out-of-order completion.
/// Returns the `(ordinal, prompt)` pairs in the order the runtime EMITTED them and the
/// workflow's own result array. Each agent resolves to its own prompt so the result
/// array position-check is independent of completion order.
async fn run_parallel(width: usize, response_order: &[usize]) -> (Vec<(u64, String)>, Vec<String>) {
    let source = format!(
        "export const meta = {{ name: 'demo', description: 'demo' }};\n\
         const thunks = [];\n\
         for (let i = 0; i < {width}; i++) thunks.push(((k) => () => agent('p' + k))(i));\n\
         const results = await parallel(thunks);\n\
         text(JSON.stringify(results));\n",
    );
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (runtime_tx, _ctrl, _handle) = spawn_runtime(
        HashMap::new(),
        workflow_request(&source),
        event_tx,
        PendingRuntimeMode::Continue,
        /*task_failure_handler*/ None,
    )
    .unwrap();

    // Collect all `width` synchronously-dispatched AgentCalls before resolving any,
    // recording emission order; a parallel batch fires every thunk (and thus every
    // `agent()`) up to the first await, so all AgentCalls arrive before responses.
    let mut emitted: Vec<(u64, String)> = Vec::new();
    let mut pending: Vec<(String, serde_json::Value)> = Vec::new();
    while emitted.len() < width {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed");
        if let RuntimeEvent::AgentCall {
            id,
            ordinal,
            prompt,
            ..
        } = event
        {
            emitted.push((ordinal, prompt.clone()));
            pending.push((id, json!(prompt)));
        }
    }

    // Resolve in the (out-of-order) completion permutation.
    for &idx in response_order {
        let (id, result) = pending[idx].clone();
        runtime_tx
            .send(RuntimeCommand::ToolResponse { id, result })
            .unwrap();
    }

    // Drain the remaining events to the terminal Result, collecting text output.
    let mut text = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("runtime event timeout")
            .expect("runtime event channel closed");
        match event {
            RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text: t }) => {
                text.push(t);
            }
            RuntimeEvent::Result { error_text, .. } => {
                assert!(
                    error_text.is_none(),
                    "fan-out must run cleanly: {error_text:?}"
                );
                break;
            }
            _ => {}
        }
    }
    (emitted, text)
}

#[tokio::test]
async fn random_parallel_widths_stamp_ordinals_in_source_order_despite_completion_order() {
    // Over many random fan-out widths resolved in random completion orders, the emitted
    // `AgentCall` ordinals are ALWAYS the strict source-order `0..N` and the result array is
    // position-preserving — ordinal assignment is keyed on dispatch order, never completion.
    let mut state: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..40 {
        let width = 1 + (next_rand(&mut state) as usize) % 8; // 1..=8
        let order = permutation(width, &mut state);
        let (emitted, text) = run_parallel(width, &order).await;

        let ordinals: Vec<u64> = emitted.iter().map(|(ordinal, _)| *ordinal).collect();
        let expected: Vec<u64> = (0..width as u64).collect();
        assert_eq!(
            ordinals, expected,
            "ordinals must be source-order 0..N regardless of completion order {order:?}",
        );
        let prompts: Vec<&str> = emitted.iter().map(|(_, p)| p.as_str()).collect();
        let expected_prompts: Vec<String> = (0..width).map(|k| format!("p{k}")).collect();
        assert_eq!(
            prompts,
            expected_prompts
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            "the ordinal->prompt mapping follows source order",
        );

        // The workflow result is position-preserving: results[k] == the k-th prompt, even
        // though the agents settled in `order`.
        let expected_json = serde_json::to_string(&expected_prompts).unwrap();
        assert_eq!(
            text,
            vec![expected_json],
            "results stay position-preserving"
        );
    }
}

#[tokio::test]
async fn same_shape_stamps_identical_ordinals_under_different_completion_orders() {
    // Determinism: the SAME fan-out shape run twice under DIFFERENT completion orders emits a
    // byte-identical `(ordinal, prompt)` sequence — the cache spine is reproducible run to run,
    // which is the precondition for a resumed run's keys to match the journaled ones.
    let width = 6;
    let (emitted_a, _) = run_parallel(width, &[0, 1, 2, 3, 4, 5]).await;
    let (emitted_b, _) = run_parallel(width, &[5, 4, 3, 2, 1, 0]).await;
    assert_eq!(
        emitted_a, emitted_b,
        "completion order must not perturb the invocation-ordinal spine",
    );
}
