export const meta = {
  name: 'dw-m3-determinism-resume',
  description: 'M3 Determinism & resume (epic #23): isolate determinism harden + (prompt,opts) journal + prefix-replay resume, in five dependency stages',
  phases: [
    { title: 'Stage 1', detail: 'leaves: native-deletes, storage-layout, journal-key, runtime-replay-state, budget-readd' },
    { title: 'Stage 2', detail: 'determinism-prelude, journal-recorder, workflow-runs-index, journal-replay-read' },
    { title: 'Stage 3', detail: 'journal-write-integration' },
    { title: 'Stage 4', detail: 'resume-prefix-loop, resume-entry' },
    { title: 'Stage 5', detail: 'uat-resume-gate (UAT-6 + determinism/replay tests)' },
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

const GROUND = `GROUND TRUTH (M0-M2 committed; read to confirm, do not re-derive):
- Workflow isolate globals gate on RuntimeState.workflow (bool threaded ExecuteRequest->RuntimeConfig). phase()/log()/agent()/parallel()/pipeline()/args/workflow.runId/budget/workflow() all install in the workflow-gated block of code-mode/src/runtime/globals.rs.
- agent() emits RuntimeEvent::AgentCall{id,ordinal,prompt,opts}; the ordinal is stamped synchronously in agent_callback (code-mode/src/runtime/callbacks.rs) BEFORE returning the promise; opts is AgentCallOpts. AgentCall routes cell_actor -> host delegate -> AgentControl::spawn_and_await_final_message, resolved by id as AgentSpawnOutcome{Completed,Failed,Rejected}. Each child registers a thread (child_thread_id) with its own rollout_path.
- The codex-workflow-journal crate already exists (committed): JournalLine{run_meta,agent_call,phase,log} + WorkflowRunMeta + AgentCallLine (with child_thread_id/rollout_path/tokens_spent/status/return) + a null-only NullOrdinal on phase/log + validate(). Reuse it; do not redefine the types.
- rollout crate: RolloutRecorder (background mpsc JSONL writer with monotonic ordinal_state), ReverseJsonlScanner (tail/prefix reads). RolloutBudget has spent()/remaining()/reserve()/reset() and the resettable configure cell. runId is minted host-side (uuid v7) and exposed via workflow.runId (#20).
- The hermetic UAT harness is core/tests/suite/workflow_uat.rs (in-process lane, Feature::CodeModeHost disabled).`

const buildPrompt = (t, priorInStage) => `You are implementing one ticket of Dynamic Workflows milestone M3 (Determinism & resume, GitHub epic #23) in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). This is the highest-novelty, risk-gated milestone (R1: resume ships experimental until the harden + journal/replay are all green) — correctness and DETERMINISM are paramount.

${GROUND}

Ticket: ${t.id}.

READ FIRST: (1) ${REPO}/docs/dynamic-workflows-plan.md section "#### \\\`${t.id}\\\`" — its description + "_Acceptance:_" exactly; (2) spec ${REPO}/docs/dynamic-workflows-spec.md §7 (Determinism, journal & resume) — the authoritative design, esp. the invocation-ordinal cache spine, the (prompt,opts) key, the journal format, and the resume algorithm; (3) the named code precedents before writing code.

TICKET-SPECIFIC: ${t.extra}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command. RUST_MIN_STACK=8388608 for codex-core integration binaries.
- Only modify files within this scope: ${t.scope}. Other agents in THIS stage edit OTHER areas of the shared working tree concurrently — never touch their files, never revert their changes, NEVER run git commit/add/push/stash/checkout. Prior-stage work is already in the tree; build on it, do not redo it.
- DETERMINISM IS THE POINT: never add Date/Math/rand/wall-clock/setTimeout on the workflow path; timestamps/ids come host-side only; the cache key excludes label/phase; ordinals key the cache, never completion order.
- Add the tests the acceptance criteria name; iterate targeted 'cargo test -p <package>' until green; scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. cargo may block on the target-dir lock — wait it out.
- Do NOT change product behavior just to make a test pass; if a test reveals a real bug, note it. If blocked, return status "blocked" with notes.
Return the structured result; files_changed = every file you created or modified (repo-relative).`

const runStage = async (phaseName, tickets) => {
  log(phaseName + ': ' + tickets.map((t) => t.id).join(', '))
  const res = (await parallel(tickets.map((t) => () => agent(buildPrompt(t), { label: t.id, phase: phaseName, schema: RESULT, model: 'opus' })))).filter(Boolean)
  log(phaseName + ' done: ' + res.filter((r) => r.status === 'done').length + '/' + tickets.length)
  return res
}

phase('Stage 1')
const s1 = await runStage('Stage 1', [
  {
    id: 'P3-determinism-native-deletes',
    scope: 'codex-rs/code-mode/src/runtime/globals.rs (install_globals delete-list) + tests, workflow-gated so plain code-mode exec is unaffected',
    extra: 'Extend the native delete-list (currently deletes console/Atomics/SharedArrayBuffer/WebAssembly) for WORKFLOW runs to also delete WeakRef and FinalizationRegistry (GC-order nondeterministic), and REMOVE setTimeout/setInterval/clearTimeout/clearInterval from the workflow isolate entirely (wall-clock timers make the command loop interleave nondeterministically — spec §7). Gate strictly to workflow runs; plain code-mode exec keeps its timers/globals. Tests: in a workflow run, WeakRef/FinalizationRegistry/setTimeout are undefined; in plain exec they remain.',
  },
  {
    id: 'P3-storage-layout',
    scope: 'a new storage-layout module in codex-rs/core (or codex-workflow-journal) computing $CODEX_HOME/workflows/runs/<runId>/{journal.jsonl,script.js,meta.json} paths + dir creation; tests',
    extra: 'Implement the per-run storage layout: $CODEX_HOME/workflows/runs/<runId>/ with journal.jsonl (source of truth), script.js (the executed program), meta.json. Mirror rollout\'s per-run file layout. Provide path helpers + safe dir creation. runId comes from workflow.runId (host-minted uuid v7). Tests over a temp CODEX_HOME.',
  },
  {
    id: 'P3-journal-key',
    scope: 'a key.rs module in the codex-workflow-journal crate + tests',
    extra: 'blake3 canonical (prompt,opts) cache key: key = blake3(canonical_json({prompt, model, effort, agentType, isolation, schema})) with SORTED object keys and a STABLE JSON-Schema serialization. label and phase are EXCLUDED (cosmetic re-labeling must not bust cache). Add key_algo_version (stored in run_meta) so hash changes across versions are detectable. Tests: same (prompt,opts) -> same key; different label/phase -> SAME key; different model/effort/schema -> different key; key_algo_version present.',
  },
  {
    id: 'P3-runtime-replay-state',
    scope: 'codex-rs/code-mode/src/runtime/mod.rs (RuntimeState) + tests',
    extra: 'Add to RuntimeState the replay scaffolding (no dispatch behavior yet): a replay cache (ordinal-keyed prior journal entries), a replay-only budget accumulator, and a replay_active flag (true when resuming, set false permanently at first divergence). Just the state fields + init + accessors that P3-resume-prefix-loop will drive. Keep next_agent_ordinal semantics intact.',
  },
  {
    id: 'P3-budget-readd',
    scope: 'codex-rs/core/src/rollout_budget.rs + tests',
    extra: 'Add a replay-only add_spent(tokens) path so that during prefix replay the journaled tokens_spent per cached agent_call can be re-added to the weighted counter, making spent()/remaining() and the ceiling-throw boundary BYTE-IDENTICAL between the original and resumed runs (spec §7). Must not interfere with normal record_usage accounting. Test: add_spent reproduces the same spent()/remaining() as a live run at the same ordinal.',
  },
])

phase('Stage 2')
const s2 = await runStage('Stage 2', [
  {
    id: 'P3-determinism-prelude',
    scope: 'codex-rs/code-mode/src/runtime/ (a frozen JS bootstrap prelude compiled as a classic v8::Script run BEFORE evaluate_main_module, for workflow runs only) + globals.rs plumbing + tests',
    extra: 'A FROZEN JS determinism prelude, run before the main module for workflow runs: replace Math.random with a THROWING stub (opt-in args.seed-derived splitmix64 PRNG only if explicitly requested); replace Date.now with a throw; wrap the Date constructor so ARGLESS new Date()/Date() throw while explicit-arg new Date(x) and Date.parse SURVIVE (do the argless-vs-args distinction in JS). Builds on P3-determinism-native-deletes (WeakRef/FinalizationRegistry/timers already gone). Tests: Date.now()/new Date()/Math.random() throw; new Date(0)/Date.parse work; args.seed PRNG deterministic if requested.',
  },
  {
    id: 'P3-journal-recorder',
    scope: 'a recorder.rs in the codex-workflow-journal crate (append-only journal.jsonl) + tests',
    extra: 'JournalRecorder: append-only journal.jsonl reusing RolloutRecorder\'s proven machinery (background mpsc JSONL writer, monotonic ordinal_state, newline-terminated append discipline). Writes line 0 = run_meta, then agent_call/phase/log JournalLines. Timestamps host-supplied (never from the isolate). Uses the P3-storage-layout paths. Call validate() before appending (per the committed journal types). Tests: append+read round-trip; ordering; crash-safe newline discipline.',
  },
  {
    id: 'P3-workflow-runs-index',
    scope: 'codex-rs/core (a codex-state migration + model following state/src/model/agent_job.rs + state/src/lib.rs per-DB conventions) for a workflow_runs table + tests',
    extra: 'A workflow_runs SQLite discovery index {runId, name, scriptHash, scriptPath, parentRunId, status, created_at} PURELY for discovery-by-name. Replay never needs SQLite (JSONL is authoritative); this is a rebuildable projection. Follow the codex-state migration + model conventions. Tests: insert/lookup by name; rebuildable.',
  },
  {
    id: 'P3-journal-replay-read',
    scope: 'a replay.rs in the codex-workflow-journal crate (tail-first prefix read via ReverseJsonlScanner + hash validation) + tests',
    extra: 'replay.rs: load a prior journal tail-first via ReverseJsonlScanner into entries[0..M]; validate script_hash/args_hash/key_algo_version (a structural change simply produces early divergence, not an error). Return the ordered replay entries + validated run_meta. Uses the P3-journal-key algo for per-entry keys. Tests: prefix read reconstructs ordinals in order; hash-mismatch surfaces as divergence signal.',
  },
])

phase('Stage 3')
const s3 = await runStage('Stage 3', [
  {
    id: 'P3-journal-write-integration',
    scope: 'codex-rs/core/src/tools/code_mode/delegate.rs (SpawnAgent dispatch) + the phase/log emit path + wiring the JournalRecorder; tests',
    extra: 'Wire JournalRecorder into the live SpawnAgent dispatch: on each agent() finalize, record an agent_call JournalLine with ordinal, key (P3-journal-key), prompt_hash, opts, phase/label, child_thread_id, rollout_path, status, return, tokens_spent. Also record phase()/log() lines from their emit path (P0-phase-log-globals events). Journal writes go through P3-journal-recorder to the P3-storage-layout path. Integration test (fixture lane): a fan-out produces a journal.jsonl with one agent_call per subagent carrying child_thread_id + rollout_path + return + tokens_spent, reconstructable from the journal alone.',
  },
])

phase('Stage 4')
const s4 = await runStage('Stage 4', [
  {
    id: 'P3-resume-prefix-loop',
    scope: 'codex-rs/code-mode/src/runtime/callbacks.rs (agent_callback replay branch) + cell_actor/delegate resolve path + RuntimeState replay wiring; tests',
    extra: 'The core resume mechanic (spec §7 resume algorithm step 3): in agent_callback at ordinal i with computed key k — if replay_active && i<M && entries[i].key==k && entries[i].status==completed: resolve the promise from entries[i].return (reuse the resolve path), append the entry to the new journal, and re-add entries[i].tokens_spent to the budget (P3-budget-readd) so spent()/remaining() and the ceiling throw land at the identical ordinal — WITHOUT spawning. Else: set replay_active=false permanently and dispatch live. A parallel batch issues ordinals synchronously; each is served from cache the instant its ordinal is issued (barrier/no-barrier reproduced without special-casing). Tests: a replay with an unchanged prefix resolves cached (no spawn) up to first divergence, then goes live; budget re-add makes the throw boundary identical.',
  },
  {
    id: 'P3-resume-entry',
    scope: 'codex-rs/core/src/tools/code_mode/workflow_handler.rs (resumeFromRunId entrypoint) + wiring; tests',
    extra: 'resumeFromRunId entrypoint (spec §7 steps 1-2): load the prior journal tail-first (P3-journal-replay-read), validate script_hash/args_hash/key_algo_version, mint a FRESH runId for the resumed run (itself resumable), seed RuntimeState with replay_entries + replay_active=true (P3-runtime-replay-state), then run. Uses P3-workflow-runs-index for discovery + P3-storage-layout paths. Tests: resuming a completed run replays its prefix; the resumed run gets a fresh runId and is itself journaled/resumable.',
  },
])

phase('Stage 5')
const s5 = await runStage('Stage 5', [
  {
    id: 'P3-uat-resume-gate',
    scope: 'codex-rs/core/tests/suite/workflow_uat.rs (UAT-6) + determinism/replay determinism unit tests across the touched crates',
    extra: 'UAT-6 + the journal/replay determinism gate (M3 exit): on the hermetic fixture lane, (a) a workflow run produces a deterministic journal.jsonl; (b) resuming it with an UNCHANGED script replays the longest prefix from cache (assert NO re-spawn for cached ordinals via the engine artifact — subagent arrival log) and the resumed run is byte-identical (same returns, same budget spent()/remaining() at each ordinal, same ceiling-throw ordinal); (c) a CHANGED prefix diverges live at exactly the first changed ordinal; (d) determinism shims hold (Date.now/argless Date/Math.random throw; WeakRef/FinalizationRegistry/setTimeout absent). Assert engine artifacts, not model free-text.',
  },
])

phase('Verify')
const done = [...s1, ...s2, ...s3, ...s4, ...s5]
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo, RUST_MIN_STACK=8388608 for codex-core), M3 determinism+journal+resume was just implemented as uncommitted working-tree changes on top of committed M0-M2:\n${JSON.stringify(done.map((r) => ({ ticket: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 300) })), null, 2)}\n\nRECONCILE any concurrent-edit churn and FIX any breakage WITHOUT changing intended behavior:\n1. cargo check --workspace (catch cross-crate exhaustive-match breaks + seam mismatches)\n2. cargo fmt; 'cargo fmt --check' passes\n3. cargo clippy --all-targets for codex-core, codex-code-mode, codex-code-mode-protocol, codex-workflow-journal\n4. cargo test -p codex-workflow-journal; cargo test -p codex-code-mode; targeted codex-core tests (workflow_uat incl. UAT-6, delegate, rollout_budget, and the storage/index tests)\n5. Confirm intended behavior: determinism shims throw/absent in workflow runs (not plain exec); journal.jsonl is written with child_thread_id+rollout_path+tokens_spent; resume replays the unchanged prefix from cache (no re-spawn) and is byte-identical incl. budget; first divergence goes live.\nNEVER git commit/add/push. Return structured result, ticket "m3-verify"; tests_passed=true only if all green (modulo the pre-existing rollout_budget full-suite stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
