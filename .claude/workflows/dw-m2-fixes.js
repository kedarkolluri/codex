export const meta = {
  name: 'dw-m2-fixes',
  description: 'M2 review fixes: budget reservation/production-wiring/cell-semantics (core), workflow() process-host bridge + budget-handle threading + pipeline proxy-safe cap (code-mode), then UAT hardening',
  phases: [
    { title: 'Fix', detail: 'core-correctness + codemode-bridge across the crate seam' },
    { title: 'Tests', detail: 'harden UAT-5/7/10 against the fixed real wiring' },
    { title: 'Verify', detail: 'reconcile seam, fmt/clippy/test sweep' },
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

const SEAM = `SHARED SEAM CONTRACT (Agent-CORE and Agent-BRIDGE must interlock on these; the verify pass reconciles):
1. Production budget handle: the code-mode runtime already accepts an optional WorkflowBudgetHandle (spawn_runtime_with_budget), but production spawn_runtime passes None so budget.spent()/remaining() are static in real runs. Add an optional accessor on the host/session delegate chain (e.g. CodeModeSessionDelegate::budget_handle(&self) -> Option<Arc<dyn WorkflowBudgetHandle>>, default None) that Agent-BRIDGE threads into the production spawn_runtime path; Agent-CORE implements it (a core-side WorkflowBudgetHandle over the shared RolloutBudget, exposed from the CoreTurnHost/broker that already provides spawn_agent).
2. Nested workflow() on the process host: mirror the agent() spawn bridge. Add DelegateRequest::SpawnWorkflow{name, args, ...} + DelegateResponse::WorkflowSpawned{outcome: AgentSpawnOutcome} to host/message.rs, implement RemoteDelegate::spawn_workflow (code-mode-host) to round-trip it, and route it to the delegate's spawn_workflow (the in-process core handler already exists). Reuse AgentSpawnOutcome{Completed,Failed,Rejected}.
Agent-CORE owns core/** (rollout_budget.rs, workflow_handler.rs, delegate.rs). Agent-BRIDGE owns code-mode/**, code-mode-protocol/**, code-mode-host/**. Neither edits the other's crates except the seam trait method signatures above.`

const FIXES = [
  {
    id: 'fix-m2-core-budget',
    label: 'core budget correctness + wiring',
    scope: 'codex-rs/core/src/rollout_budget.rs, codex-rs/core/src/tools/code_mode/delegate.rs, codex-rs/core/src/tools/code_mode/workflow_handler.rs (+ sibling *_tests.rs)',
    findings: `You own the CORE side. Fix these VERIFIED findings (all confirmed against the code):
1. BLOCKER (delegate.rs ~581/600) — budget admission is an unreserved read of remaining(): N concurrent parallel()/pipeline() agents all see remaining()>0 before any child records usage, so the ceiling is exceeded by up to N in-flight turns (the "one in-flight turn" comment is false). FIX: add an atomic reservation to RolloutBudget (e.g. reserve(estimate)->bool that fails when it would cross limit_tokens, plus release/settle on turn completion reconciling the estimate against actual record_usage) so concurrent admissions SERIALIZE against the ceiling. Admission (CoreTurnHost::spawn_agent) reserves before spawning and settles/releases on finalize (success AND failure). Keep the overshoot bound to "at most one in-flight turn PER concurrency slot" or tighter; document the exact bound you achieve.
2. BLOCKER (workflow_handler.rs ~604) — the TOP-LEVEL workflow handler hardcodes args = Value::Null, so a top-level run's budget can never come from args.budget.total. FIX: thread the real invocation args from the tool call into the top-level execute path (and into configure_workflow_budget + the run). (Nested path at ~490 already threads args.)
3. BLOCKER (workflow_handler.rs ~145 workflow_budget_total) — total<=0 returns None (treated as unmetered), so {budget:{total:0}} admits agents instead of throwing at remaining()==0. FIX: distinguish ABSENT (no budget key -> unmetered) from an explicit total==0 (a real zero ceiling that rejects the first agent()). Negative totals -> reject as invalid or clamp to 0 per spec; document.
4. BLOCKER (workflow_handler.rs ~129 configure_workflow_budget) — an absent budget early-returns WITHOUT resetting the cell, so a session-configured or prior nested child's limit stays live and an "unmetered" workflow rejects agents. FIX: an absent budget must RESET the RolloutBudget cell to unmetered (add a reset/clear path to rollout_budget.rs if none exists).
5. BLOCKER (workflow_handler.rs ~490 nested run) — nested workflow() overwrites the single shared budget cell with no save/restore: a child can raise the parent ceiling, concurrent children race last-writer-wins, and the child's limit persists into later parent turns. FIX: snapshot the budget config+counters before a nested run and restore after (or scope the budget per-run); ensure concurrent nested children cannot corrupt each other or the parent. Preserve accumulated parent spend.
6. CLAIM-9 (workflow_handler.rs ~392/446) — the one-level guard uses the configurable agent_max_depth, so agent_max_depth=2 admits workflow depth 2 (test at ~848 asserts this). FIX: hard-cap workflow() nesting at exactly ONE level regardless of agent_max_depth (min(1, ...) semantics), per the spec non-goal "workflow() is one level deep only". Update the test to assert depth-2 is ALWAYS rejected.
7. MAJOR (workflow_handler.rs ~533) — RuntimeResponse::Yielded is treated as a terminal nested result, resolving the parent promise with partial/null output while the child isolate/callbacks keep running (leak). FIX: do not treat Yielded as terminal; drive the nested run to genuine completion (or reject clearly) and don't leak the dispatch/ledger entry.
8. MAJOR (workflow_handler.rs ~481) — nested execution does an unbounded read_to_string of the saved workflow file (registry discovery is bounded, execution is not) — a small-manifest/huge-body file OOMs. FIX: bound the execution read the same way discovery does (the core-workflows loader already has read_meta_prefix / MAX_META_READ_BYTES patterns to mirror; a workflow body still needs a hard total-size cap).
9. MAJOR reporting (delegate.rs ~589/761) — budget ThreadGoal status is emitted only when the NEXT agent() is attempted (a final over-ceiling child that ends the run never emits BudgetLimited); token_budget is reported as spent+remaining so an overshot 1000-limit with 1500 spent reports 1500; the "workflow budget" ThreadGoalUpdated uses turn_id:None + zero timestamps, is never completed/restored, and blindly overwrites the user's real goal. FIX: emit BudgetLimited when spend crosses the ceiling (not only at next admission); report token_budget as the configured limit; and either stop hijacking the user ThreadGoal or scope+restore it so a completed workflow doesn't leave a stale budget goal.
10. MAJOR (rollout_budget.rs ~23) — reminder_at_remaining_tokens=[] does not suppress the initial reminder (pending_reminder returns index 0), so parallel children get completion-order-dependent remaining-token fragments injected into model context (nondeterministic). FIX: actually suppress reminders for the workflow output-weight config so no schedule-dependent context is injected.
Provide the core-side production WorkflowBudgetHandle impl + the seam accessor (see SEAM #1). Coordinate with Agent-BRIDGE only via the SEAM trait methods.`,
  },
  {
    id: 'fix-m2-bridge',
    label: 'codemode/host bridge wiring',
    scope: 'codex-rs/code-mode/src/runtime/globals.rs, runtime/mod.rs, cell_actor/**, session_runtime/**, service.rs; codex-rs/code-mode-protocol/src/session.rs, host/message.rs; codex-rs/code-mode-host/src/delegate.rs, lib.rs, peer.rs (+ their tests)',
    findings: `You own the CODE-MODE / PROTOCOL / HOST bridge side. Implement the SEAM (both halves) so budget + workflow() work on the DEFAULT process host, and fix the pipeline cap.
1. BLOCKER (cell_actor/mod.rs ~68 -> runtime/mod.rs ~168) — production spawn_runtime passes budget: None, and the only WorkflowBudgetHandle impl is the test FixtureBudget, so real JS always sees budget.spent()==0 / remaining()==static total. FIX: thread the delegate's budget_handle() (SEAM #1) through the production spawn_runtime path so a real workflow cell gets the live handle. (Agent-CORE provides the impl + accessor.)
2. BLOCKER (session.rs ~162 default spawn_workflow -> Failed; host/message.rs ~218 no wire variant; code-mode-host/delegate.rs ~29 no impl) — nested workflow() is absent from the default process-host wire, so under Feature::CodeModeHost (default) await workflow(child) resolves to null. FIX: add the SpawnWorkflow wire request/response (SEAM #2) + RemoteDelegate::spawn_workflow round-trip + host routing to the delegate's spawn_workflow, mirroring the agent() SpawnAgent bridge exactly. AgentSpawnOutcome{Completed,Failed,Rejected} round-trips over the wire (reuse WireAgentSpawnOutcome).
3. MAJOR (globals.rs ~172) — the pipeline() 4096 cap is bypassable with an array Proxy that returns a small length to the guard then a larger length when .map() rereads it. FIX: make the cap check single-read / proxy-safe — snapshot the validated ordinary-array length and contents (e.g. Array.prototype.slice.call into a real array, or validate Array.isArray + a captured length used for iteration) before dispatch so stages cannot dispatch for >4096 positions. Apply the same hardening to parallel()'s cap if it shares the pattern. Add an in-isolate test with a length-lying Proxy asserting no dispatch beyond the cap.
Coordinate with Agent-CORE only via the SEAM trait methods; do not edit core/**.`,
  },
]

const buildPrompt = (f) => `You are applying VERIFIED code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). The tree holds Dynamic Workflows M2 (budget governance, pipeline, workflow() nesting) on top of committed M0+M1. An independent audit CONFIRMED every finding below against the code — they are real; re-verify locally as you fix and push back in notes only with evidence. Overarching theme: the budget hard-ceiling has a concurrency hole (no reservation) AND, like agent() before it, the budget JS global + nested workflow() are inert on the DEFAULT process host (Feature::CodeModeHost). Complete the correctness + wiring.

${SEAM}

Your fix bundle: ${f.id} (${f.label})

${f.findings}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command. RUST_MIN_STACK=8388608 for codex-core.
- Only modify files within this scope: ${f.scope}. The OTHER fix agent edits the other side of the seam concurrently — never edit their crates, never revert their work; rely on the SEAM contract + verify pass to reconcile.
- NEVER git commit/add/push/stash/checkout. No wall-clock timeouts/sleeps in product code; no Date/Math/rand on the workflow path.
- Add/extend tests; iterate targeted 'cargo test -p <package>' until green; scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean for your crates. If the crate won't compile because the other agent's seam half isn't in yet, get YOUR side internally consistent, note it, and return done.
- If genuinely blocked, return status "blocked" with notes.
Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 2 M2 fix agents across the crate seam (core budget vs codemode/host bridge)')
const fixResults = (await parallel(FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' })))).filter(Boolean)
log('Fixes done: ' + fixResults.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Tests')
const testAgent = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo, RUST_MIN_STACK=8388608 for codex-core), the M2 budget + workflow()-nesting wiring was just FIXED (budget now reserves at admission, is sourced from real args, treats total==0 as a zero ceiling, resets when absent, saves/restores around nested runs; the budget JS global + nested workflow() now work on the default process host; workflow() nesting is hard-capped at one level; the pipeline 4096 cap is proxy-safe). Fix reports:\n${JSON.stringify(fixResults.map((r) => ({ fix: r.ticket, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nThe M2 UAT gates in core/tests/suite/workflow_uat.rs were flagged as bypassing the real wiring. HARDEN them (assert engine artifacts only, no vacuous/tautological checks):\n- UAT-5 (budget): drive a REAL args.budget.total through the tool call, read the live budget JS global (spent()/remaining() must reflect real accrual, not static), assert the ceiling throws BudgetExceeded, and prove ADMISSION ORDER — saturate a competing gate (lifetime/permit) and show BudgetExceeded wins without consuming it; add a CONCURRENCY test proving a parallel() fan-out cannot exceed the ceiling by more than the documented bound (the pre-fix hole let N turns through).\n- UAT-7 (pipeline no-barrier): replace the fixed wall-clock delay with a fixture LATCH so the staggered-order artifact is deterministic on slow CI (not timing-flaky).\n- UAT-10 (workflow nesting): assert on the nested-run linkage the spec requires (parent_run_id in the journal/ledger, not just the child rollout's parent_thread_id); run at least one case through the DEFAULT process-host path (not only the in-process lane); and cover budget lifecycle across nesting (shared-spend preserved, parent ceiling restored after the child, absent-budget child, concurrent children).\nAlso add/repair unit tests so budget admission-order is asserted against the REAL CoreTurnHost::spawn_agent predicate (not a test-only copy) and the parallel-overshoot bound is asserted concurrently.\nDo NOT weaken assertions to pass; if a fix is incomplete, FAIL loudly and report it. Scope: core/tests/suite/workflow_uat.rs + relevant sibling *_tests.rs. NEVER git commit/add/push. Return the structured result, ticket "m2-uat-harden".`,
  { label: 'uat-harden', phase: 'Tests', schema: RESULT, model: 'opus' }
)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo, RUST_MIN_STACK=8388608 for codex-core), two coupled fix agents + a test-hardening agent just reworked M2 budget governance + workflow() nesting across a crate seam (production WorkflowBudgetHandle threading + SpawnWorkflow process-host wire bridge). RECONCILE the seam so both halves link, then FIX any breakage WITHOUT changing intended behavior:\n1. cargo check --workspace (catch seam mismatches + cross-crate exhaustive-match breaks from the new SpawnWorkflow wire variant — check the TUI/app-server match sites too)\n2. cargo fmt; 'cargo fmt --check' passes\n3. cargo clippy --all-targets for codex-core, codex-code-mode, codex-code-mode-protocol, codex-code-mode-host\n4. cargo test -p codex-code-mode; targeted codex-core tests (rollout_budget, workflow_uat UAT-5/7/10, delegate, workflow_handler, scheduler)\n5. Confirm intended behavior end-to-end: budget.spent()/remaining() are LIVE in a real run; a concurrent parallel() fan-out cannot exceed the ceiling by more than the documented bound; total==0 rejects the first agent(); an absent budget is unmetered (resets a stale cell); nested workflow() runs on the default process host and is capped at one level; nested budget is restored after the child.\nNEVER git commit/add/push. Return structured result, ticket "m2-fix-verify"; tests_passed=true only if all green (modulo the pre-existing rollout_budget stack overflow in the full core suite).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { fixes: fixResults, tests: testAgent, verify }
