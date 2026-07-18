export const meta = {
  name: 'dw-wave2-fixes3',
  description: 'Final convergence fixes for the wave-2 narrow re-review: spawn_await ordering/steering, loader traversal/utf8/bucket, error-bound Ok-path + marker budget + wire back-compat',
  phases: [
    { title: 'Fix', detail: 'three parallel fix agents over disjoint findings' },
    { title: 'Verify', detail: 'fmt/clippy/test sweep' },
  ],
}

const REPO = '/home/kedar/projects/codex/codex'

const RESULT = {
  type: 'object',
  additionalProperties: false,
  required: ['ticket', 'status', 'files_changed', 'test_commands', 'tests_passed', 'summary'],
  properties: {
    ticket: { type: 'string' },
    status: { type: 'string', enum: ['done', 'blocked'] },
    files_changed: { type: 'array', items: { type: 'string' } },
    test_commands: { type: 'array', items: { type: 'string' } },
    tests_passed: { type: 'boolean' },
    summary: { type: 'string' },
    notes: { type: 'string' },
  },
}

const FIXES = [
  {
    id: 'fix3-spawn-await-ordering',
    scope: 'codex-rs/core/src/agent/control/spawn_await.rs, spawn_await_tests.rs (and read-only study of session dispatch steer_input / send_input to understand steering)',
    findings: `Two verified findings on the spawn_await helper (current working tree):
1. P1 (spawn_await.rs ~150/167) — foreign active turn defeats submission-id filtering. If a client starts a turn during the announce->submit window, send_input returns a NEW submission id, but session dispatch STEERS that input into the already-active turn and discards the active-turn id from steer_input. The terminal event then carries the FOREIGN id and our id-filter drops it: without lag the helper hangs; after lag the status fallback can return the foreign turn's message. FIX: obtain the id of the turn our input actually runs under (the active/steered turn id, not just the submission id) and filter terminal events on THAT. Investigate send_input/steer_input return values and session dispatch to capture the correct turn id; if the API doesn't expose it, the deferred spawn already guarantees no turn is running when WE submit — ensure our submission genuinely starts a fresh turn (no foreign steering) or capture whatever id the resulting TurnComplete will carry. Add a regression test simulating a foreign turn submitted in the window and assert the helper still returns OUR result (or None), never a foreign message and never hangs.
2. P1 (spawn_await.rs ~197) — the tokio::select! is unbiased, so it may pick wait_until_terminated() even when a matching TurnComplete is already buffered/ready, returning None instead of the final message. FIX: make the select biased (biased;) with the event-tap arm FIRST, and/or on the teardown arm drain any already-ready terminal event before returning None. Add a test where a matching TurnComplete and thread termination are both ready and assert the message wins.
Determinism: no wall-clock timeouts.`,
  },
  {
    id: 'fix3-loader-traversal-bucket-utf8',
    scope: 'codex-rs/core-workflows/src/loader.rs, tests/loader.rs',
    findings: `Three verified findings on the bounded loader traversal (current working tree):
1. P2 (loader.rs ~334) — bounding the SUBDIRECTORY bucket to limit+1 silently loses candidates: subdirs are not candidates themselves, so with 258 subdirs where only the last contains a workflow and the first 257 are empty, discovery returns nothing AND no truncation diagnostic. FIX: do NOT cap the subdirectory set by the candidate limit — only the FILE candidate count should be bounded. Recurse into subdirs (in deterministic sorted order) until the global candidate limit is hit; the cap must reflect actual discovered workflow files, not directory fan-out. Keep memory bounded per directory without dropping unexplored subtrees that could contain candidates within the global budget.
2. P2 (loader.rs ~271) — buffer length == cap does not prove a clipped multibyte sequence: a file exactly MAX_META_READ_BYTES ending in an incomplete lead byte is wrongly accepted-and-truncated, and a longer file whose next (unread) byte would be an invalid continuation is also accepted. FIX: only tolerate a trailing incomplete sequence when from_utf8 error is UnexpectedEof-style (error_len().is_none()) AND you can establish the truncated-at valid_up_to() prefix is the intended boundary — i.e. treat it as valid only if the incomplete sequence is a plausible lead+partial run; do not assume validity of unseen bytes. If validity cannot be established, fail-open skip with recorded error. Tighten the test to distinguish a genuinely clipped valid char from a malformed-at-EOF byte.
3. P2 (loader.rs ~322) — push_bounded caps retained memory but the loop still enumerates and file_type()-stats EVERY entry in a directory before emitting the sentinel, so a flat million-entry directory still does a million iterations/stats, contradicting the bounded-traversal guarantee. FIX: short-circuit the per-directory enumeration once enough candidates (global limit + sentinel) are collected so a pathological flat directory does not force full enumeration. Document the determinism of the early stop (readdir order is not sorted, so if you early-stop before sorting you lose determinism — resolve this: e.g. cap the RAW enumeration at a generous hard ceiling with a recorded 'directory too large' error, distinct from the sorted candidate cap, so both bounded-work AND deterministic-surviving-set hold). Prove the bound with an instrumented enumeration-count test, not timing.`,
  },
  {
    id: 'fix3-error-bounds-and-wire-compat',
    scope: 'codex-rs/core/src/tools/code_mode/workflow_handler.rs (+ its tests), and codex-rs/code-mode-protocol/src/host/payload.rs + runtime.rs (the workflow wire field only) + affected wire snapshot/round-trip tests',
    findings: `Three verified findings (current working tree):
1. P0 (workflow_handler.rs ~203/250) — bound_model_error only processes the Err(FunctionCallError) path. Script failures come back as Ok(FunctionToolOutput) carrying RuntimeResponse::Result.error_text, so a workflow can throw a dynamically-generated huge error (raising max_output_tokens) that bypasses the 2048-byte model bound; even the default allows ~10K tokens. FIX: bound the workflow's runtime error_text on the Ok/Result path too, independently, before it becomes model-visible output. Test with a large Result.error_text asserting the model-visible output is capped.
2. P2 (workflow_handler.rs ~58) — truncate_model_error retains up to MAX_MODEL_ERROR_BYTES=2048 then APPENDS "… [error truncated]", so the result exceeds the stated hard cap. FIX: reserve the marker length inside the budget so the final string length <= MAX_MODEL_ERROR_BYTES (truncate to cap - marker_len, UTF-8 boundary safe). Update the test to assert total length <= cap.
3. P1 (payload.rs ~150) — the new serde-default 'workflow' bool breaks new-client -> old-host V1: #[serde(default)] only covers old-client -> new-host; a NEW client always serializes "workflow": false, and older V1 hosts use deny_unknown_fields so they now REJECT even ordinary exec requests after a successful V1 handshake. FIX: omit the field when false — add skip_serializing_if so 'workflow: false' serializes to NOTHING (identical wire bytes to legacy for all non-workflow execs), so only workflow:true (a new capability an old host wouldn't be asked for) carries the field. Apply on both ExecuteRequest and WireExecuteRequest. Update the pinned wire snapshot so legacy/plain-exec payloads contain NO 'workflow' key, and keep the round-trip + default tests. Verify a plain exec request's serialized JSON is byte-identical to before this field existed.`,
  },
]

const buildPrompt = (f) => `You are applying the FINAL convergence code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). The tree holds the wave-2 Dynamic Workflows implementation after two prior fix rounds (plan: docs/dynamic-workflows-plan.md; spec: docs/dynamic-workflows-spec.md, §6-§7). The findings below are from a narrow codex re-review of the latest delta; verify each locally as you fix — push back in notes with evidence if one is wrong.

Your fix bundle: ${f.id}

${f.findings}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only modify files within this scope: ${f.scope}. Other agents are concurrently fixing OTHER files — never touch their areas, never revert working-tree changes you did not write, NEVER run git commit/add/push/stash/checkout.
- Never introduce wall-clock timeouts/sleeps into product code.
- Update/extend tests; iterate targeted 'cargo test -p <package>' until green; scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. RUST_MIN_STACK=8388608 for codex-core integration binaries.
- Cargo may block on the shared target-dir lock — wait it out. If genuinely blocked, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 3 final convergence fix agents')
const results = await parallel(
  FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' }))
)
const done = results.filter(Boolean)
log('Fixes done: ' + done.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), final convergence fixes were just applied to the uncommitted wave-2 changes:\n\n${JSON.stringify(done.map((r) => ({ fix: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRun and FIX any breakage (without changing intended behavior):\n1. cargo fmt; 'cargo fmt --check' passes\n2. cargo clippy --all-targets for codex-core-workflows, codex-code-mode-protocol, codex-code-mode, codex-core, codex-app-server\n3. cargo test -p codex-core-workflows -p codex-code-mode-protocol -p codex-code-mode; targeted spawn_await + workflow_handler tests (RUST_MIN_STACK=8388608); app-server workflow tests (target 'all')\n4. cargo check -p codex-core -p codex-app-server\nNEVER git commit/add/push. Return structured result, ticket "wave2-fix3-verify"; tests_passed=true only if all green (modulo the documented pre-existing rollout_budget stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
