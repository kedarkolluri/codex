export const meta = {
  name: 'dw-wave2-fixes2',
  description: 'Round-3 fixes for the wave-2 re-review: spawn_await turn identity/shutdown/capacity, event-tap clone cost, loader traversal/UTF-8, service cache race, explicit workflow mode, bounded parser errors',
  phases: [
    { title: 'Fix', detail: 'five parallel fix agents over disjoint findings' },
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
    id: 'fix2-spawn-await-hardening',
    scope: 'codex-rs/core/src/agent/control/spawn_await.rs, spawn_await_tests.rs, spawn.rs (deferred path only), and session event-tap code you added last round (session/mod.rs, session/session.rs, codex_thread.rs) — do not touch unrelated session logic',
    findings: `Re-review round-2 findings on the spawn_await helper + event tap (all in the current working tree):
1. P1 turn identity: spawn_await.rs:127 discards the submission id from send_input_after_capacity_check and :137 accepts ANY terminal event. The child is announced (notify_thread_created) before the helper submits, so a client turn submitted in that window can make the helper return the wrong message or None. FIX: capture the submission id and only accept TurnComplete/TurnAborted whose Event.id matches it.
2. P1 shutdown hang: if the session/thread is torn down while the helper waits, the tap sender can stay alive (retained thread) so RecvError::Closed never fires and the helper waits forever at :136. FIX: add a deterministic secondary wake — e.g. also watch the child's status watch channel (AgentStatus/final) or thread-removal signal, returning None when the child reaches a final state without our turn's terminal event. NO wall-clock timeouts (determinism requirement).
3. P1 lag reconciliation: Lagged=>continue can now (with the id filter) skip OUR terminal event forever. FIX: after a Lagged, reconcile — check whether the child turn already finished (status watch / thread state); if so recover last_agent_message if it is still retrievable (e.g. thread state/history) or return None; never spin forever.
4. P1 capacity: the deferred path runs the execution-capacity check at spawn_agent_internal time, then submits later — another agent can take the last slot in between. FIX: make the deferred submission re-run the same current-capacity check a public send_input would (name says it, behavior must too).
5. Test gap: the competing-drain test has no readiness handshake and never asserts the competitor drained anything. FIX: handshake so the competitor is provably draining before the turn is submitted, and assert it consumed >=1 event.
Also: session/mod.rs:1996 clones every Event into the tap even with zero receivers — a global perf cost on ALL sessions. FIX: guard on event_observers.receiver_count() > 0 before cloning (keep semantics identical when observers exist).`,
  },
  {
    id: 'fix2-loader-traversal-utf8',
    scope: 'codex-rs/core-workflows/ (loader.rs, tests)',
    findings: `1. P1: the candidate cap truncates AFTER a full unbounded walk (loader.rs:141 walks everything, :261 accumulates every match, :149 truncates) — millions of files still mean millions of fs ops/allocations. FIX: enforce the bound DURING traversal: stop collecting once the per-root cap (+1 to detect overflow) is reached, keep a running dropped-count (or 'more than N' marker) without accumulating paths, keep deterministic sorted-order semantics for the surviving set (walk in sorted order per directory, or collect bounded per-dir then merge — design it, document it).
2. P2: from_utf8_lossy admits U+FFFD anywhere in the manifest — an invalid byte inside meta.name silently enters the registry while later strict loading rejects the file. FIX: strict UTF-8 validation for the manifest region; ONLY an incomplete multi-byte sequence that straddles the exact read boundary gets boundary-specific tolerance (truncate at the last complete char); any other invalid UTF-8 = fail-open skip with recorded error.
3. P2: the truncation diagnostic says 'lowest-sorted' files were ignored but the sorted-ascending list's TAIL is truncated (highest-sorted dropped). FIX wording to match actual behavior (or change behavior to match the message — pick one, document).
4. Test gap: 'under 5s for 12MiB' does not prove bounded reading. FIX: prove the bound properly — e.g. a multi-GB sparse file (fs::File::set_len) whose full read is infeasible, or instrument the read path to count bytes and assert <= cap.`,
  },
  {
    id: 'fix2-service-cache-race',
    scope: 'codex-rs/app-server/src/workflows_service.rs (+ move its inline tests to a sibling *_tests.rs per repo convention) and workflows_watcher tests if needed',
    findings: `P1: workflows_service.rs:67 loads outside the mutex and :74 inserts the result after; clear_cache() during an in-flight load gets repopulated by the pre-change result — readers stay stale forever since the WorkflowsChanged notification already fired. FIX: generation/epoch counter — snapshot the generation before loading, only insert if the generation is unchanged (or re-check and drop). Add a deterministic test that interleaves clear_cache() during a load and asserts the stale result is NOT cached. Also: move the module's inline #[cfg(test)] mod tests to a sibling workflows_service_tests.rs included via #[path] (repo convention for NEW test modules; precedent: core/src/config/schema.rs -> schema_tests.rs). Add (if missing) a test covering the RAII WatchRegistration unregister-on-drop lifecycle.`,
  },
  {
    id: 'fix2-workflow-mode-flag',
    scope: 'codex-rs/code-mode/src/runtime/mod.rs, code-mode/src/cell_actor/, the code-mode protocol request types that carry an execute request (code-mode-protocol), core/src/tools/code_mode/workflow_handler.rs + delegate/service call path, and affected tests',
    findings: `P1: runtime/mod.rs:209 decides 'is a workflow' by re-parsing the source for a valid meta manifest — an ordinary code-mode exec whose source happens to contain 'export const meta = {...}' silently gains the phase()/log() workflow globals (and every future workflow global: agent/args/budget). FIX: make workflow-ness an EXPLICIT invocation mode, not source-sniffing: add a workflow flag (e.g. bool or enum mode field, serde-defaulted to false for backward compat) threaded from the workflow handler's execute call through ExecuteRequest/CreateCellRequest (the round-trip that previously dropped fields — extend it properly this time) into run_runtime/install_globals. The workflow handler sets it; plain exec never does. Delete the source-sniffing helper. Tests: plain exec with a meta-shaped source does NOT get phase/log; workflow handler path does; protocol round-trip test for the new field; all 85+ existing code-mode tests stay green. Search all ExecuteRequest construction sites (the wave-2 agent counted 56 literal sites — most use ..Default::default() or builders; update only what fails to compile).`,
  },
  {
    id: 'fix2-bounded-errors-and-registration-test',
    scope: 'codex-rs/core/src/tools/code_mode/ (workflow_handler.rs, mod.rs tests, spec_plan.rs read-only), codex-rs/code-mode-protocol/src/workflow_meta.rs (error-message bounding only), core/src/config/mod.rs (move the inline workflow_dependency_tests to a sibling *_tests.rs + add one Config-loading test)',
    findings: `1. P0: workflow_handler.rs:70 forwards parser errors verbatim to RespondToModel, and workflow_meta.rs error paths (:293, :381, :695) embed complete offending identifiers/numbers — a malformed near-256KiB identifier becomes a ~60K-token tool result poisoning model context. FIX both layers: (a) in workflow_meta.rs, cap echoed identifiers/tokens in error strings (e.g. 80 chars + ellipsis); (b) in workflow_handler.rs, hard-truncate any model-visible error string (e.g. 2048 bytes with a truncation marker). Tests for both bounds.
2. Registration-surface gap: handler tests construct CodeModeWorkflowHandler directly, so a spec_plan.rs:517 registration/gating regression passes. FIX: add a test that goes through build_code_mode_executors itself asserting (a) feature-enabled => the "workflow" tool is registered/dispatchable, (b) feature-disabled => absent. Mirror however spec_plan/build_code_mode_executors is tested today if precedent exists.
3. Convention: core/src/config/mod.rs:4407 added an inline workflow_dependency_tests module; move it to a sibling *_tests.rs via #[path] (precedent: config/schema.rs -> schema_tests.rs). While there, add one test that drives ACTUAL Config loading (not the validator directly) with workflow enabled + a dep explicitly disabled, asserting the resolution error surfaces — covering the validator wiring.`,
  },
]

const buildPrompt = (f) => `You are applying round-3 code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). The tree holds the wave-2 Dynamic Workflows implementation after round-2 fixes (plan: docs/dynamic-workflows-plan.md; spec: docs/dynamic-workflows-spec.md §6-§7 for the spawn/agent contract). The findings below come from a focused codex re-review; verify each locally as you fix — if one is wrong, push back in notes with evidence instead of forcing a change.

Your fix bundle: ${f.id}

${f.findings}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only modify files within this scope: ${f.scope}. Other agents are concurrently fixing OTHER files — never touch their areas, never revert working-tree changes you did not write, NEVER run git commit/add/push/stash/checkout.
- Determinism constraint: never introduce wall-clock timeouts/sleeps into product code paths.
- Update/extend tests; iterate targeted 'cargo test -p <package>' until green; scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. RUST_MIN_STACK=8388608 for codex-core integration binaries; app-server integration test target is 'all'.
- Cargo may block on the shared target-dir lock — wait it out. If genuinely blocked, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 5 round-3 fix agents')
const results = await parallel(
  FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' }))
)

const done = results.filter(Boolean)
log('Fixes done: ' + done.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), round-3 review fixes were just applied to the uncommitted wave-2 changes:\n\n${JSON.stringify(done.map((r) => ({ fix: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRun and FIX any breakage (without changing intended behavior):\n1. cargo fmt; 'cargo fmt --check' passes\n2. cargo clippy --all-targets for codex-core-workflows, codex-app-server, codex-core, codex-code-mode, codex-code-mode-protocol\n3. cargo test -p codex-core-workflows -p codex-code-mode -p codex-code-mode-protocol; targeted spawn_await + workflow handler + config workflow tests + app-server workflows tests (RUST_MIN_STACK=8388608; app-server target 'all')\n4. cargo check -p codex-core -p codex-app-server\nNEVER git commit/add/push. Return structured result, ticket "wave2-fix2-verify"; tests_passed=true only if all green (modulo the documented pre-existing rollout_budget stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
