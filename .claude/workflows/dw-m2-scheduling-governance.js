export const meta = {
  name: 'dw-m2-scheduling-governance',
  description: 'M2 Scheduling & governance (epic #22): pipeline() no-barrier, budget hard-ceiling, one-level workflow() nesting — three parallel dependency chains each ending in its UAT',
  phases: [
    { title: 'Implement', detail: 'chains: [pipeline], [budget], [workflow-nesting]' },
    { title: 'Verify', detail: 'fmt/clippy/test sweep over touched crates' },
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

// Three independent chains. Tickets within a chain run sequentially (each builds on the prior).
const CHAINS = [
  // Chain P — pipeline() no-barrier
  [
    {
      id: 'P2-pipeline-prelude', headings: ['P2-pipeline-prelude'],
      scope: 'the injected JS workflow prelude in codex-rs/code-mode/src/runtime/globals.rs (next to the parallel() prelude from #17) + in-isolate tests in runtime',
      extra: 'pipeline(items, ...stages) as pure JS, NO host op, gated in the same `if workflow {` block as parallel(): items.map(i => stages.reduce((p, s) => p.then(s), Promise.resolve(i)).catch(() => null)) — per-item promise chains, NO barrier between stages (item A can be in stage 3 while B is in stage 1), a stage throw drops THAT item to null position-preserving. Validate items.length <= 4096 before dispatch (reuse the parallel() cap-guard). Concurrency stays bounded by the host WorkflowScheduler semaphore (#18). In-isolate tests: position-preserving length; per-item independent progress (observe A at stage 3 while B at stage 1 via staggered agent() resolution); stage-throw isolates one item; >4096 throws before dispatch.',
    },
    {
      id: 'P2-test-pipeline-uat7', headings: ['P2-test-pipeline-uat7'],
      scope: 'in-isolate/unit tests for pipeline in codex-rs/code-mode + a hermetic UAT-7 in codex-rs/core/tests/suite/workflow_uat.rs (extend the existing file from #21)',
      extra: 'UAT-7 asserts the no-barrier semantic on the hermetic fixture lane (mirror the #21 UAT harness in workflow_uat.rs). Assert engine artifacts, not model free-text.',
    },
  ],
  // Chain B — budget hard-ceiling (getters + resettable-cell already landed in wave 1)
  [
    {
      id: 'P2-budget-output-weight-config', headings: ['P2-budget-output-weight-config'],
      scope: 'codex-rs/core/src/rollout_budget.rs (config wiring) and wherever the workflow run configures its RolloutBudget (the workflow handler / scheduler construction)',
      extra: 'Configure the workflow run\'s RolloutBudget with sampling_token_weight=1.0, prefill_token_weight=0.0, reminder_at_remaining_tokens=[], limit_tokens = args.budget.total, so weighted_tokens_used == pure output-token spend. Uses the resettable cell (#22, committed wave 1). The spent()/remaining() getters already exist (committed wave 1).',
    },
    {
      id: 'P2-budget-js-global', headings: ['P2-budget-js-global'],
      scope: 'codex-rs/code-mode/src/runtime/globals.rs (budget global, in the `if workflow {` block), mod.rs plumbing, and the protocol/runtime threading to carry the budget total into the isolate (follow the args/#20 plumbing)',
      extra: 'Native-backed budget JS global { total, spent(), remaining() } forwarding to RolloutBudget::spent()/remaining() (committed getters) and total from args. Thread the live budget handle/total host-side the way args/runId (#20) are threaded. Read-only. Install only when RuntimeState.workflow.',
    },
    {
      id: 'P2-budget-pre-admission-throw', headings: ['P2-budget-pre-admission-throw'],
      scope: 'codex-rs/core/src/tools/code_mode/delegate.rs (CoreTurnHost::spawn_agent admission) and scheduler.rs if the check belongs there; tests',
      extra: 'Per spec §5/§8 admission order STEP 1 (before lifetime CAS and permit): if budget.remaining() <= 0 -> return AgentSpawnOutcome::Rejected("BudgetExceeded") so agent() THROWS in JS. This slots in ahead of the existing AgentCapReached lifetime check on the live spawn path. Test: a run at/over its ceiling makes the next agent() throw BudgetExceeded (Rejected), not resolve null.',
    },
    {
      id: 'P2-budget-thread-goal-reporting', headings: ['P2-budget-thread-goal-reporting'],
      scope: 'codex-rs/core (budget reporting via ThreadGoal + BudgetLimited status) — follow the plan ticket\'s named files; keep scope tight',
      extra: 'Surface budget state via the existing ThreadGoal + BudgetLimited status path per the plan ticket. Uses the committed budget getters.',
    },
    {
      id: 'P2-test-budget-uat5', headings: ['P2-test-budget-uat5'],
      scope: 'budget unit tests in codex-rs/core/src/rollout_budget.rs (or sibling *_tests.rs) + a hermetic UAT-5 in codex-rs/core/tests/suite/workflow_uat.rs',
      extra: 'UAT-5 asserts the hard ceiling on the hermetic fixture lane: agent() throws BudgetExceeded at the ceiling; ceiling overshoots by at most one in-flight turn. Assert engine artifacts.',
    },
  ],
  // Chain W — one-level workflow() nesting
  [
    {
      id: 'P2-workflow-global-callback', headings: ['P2-workflow-global-callback'],
      scope: 'codex-rs/code-mode/src/runtime/globals.rs (workflow global, `if workflow {` block) + callbacks.rs (workflow_callback) + runtime/mod.rs (RuntimeEvent::WorkflowCall) — mirror the agent()/#10 callback structure',
      extra: 'workflow(nameOrRef, args) JS global + workflow_callback minting a PromiseResolver and emitting RuntimeEvent::WorkflowCall{id, name, args} (structurally like agent_callback/#10). No host handling yet (that is P2-workflow-registry-reenter). Gate on RuntimeState.workflow.',
    },
    {
      id: 'P2-workflow-registry-reenter', headings: ['P2-workflow-registry-reenter'],
      scope: 'codex-rs/core/src/tools/code_mode/ (workflow() host handler + cell_actor/delegate dispatch of RuntimeEvent::WorkflowCall) and the code-mode dispatch bridge; tests',
      extra: 'Host handler: resolve the named saved workflow via codex_core_workflows::resolve_by_name (committed #5 loader), load its script, and RE-ENTER the runtime nested ONE level as a nested cell/subagent, returning its top-level result to the WorkflowCall promise. Reuse the resettable budget cell (#22) for the nested run. Route RuntimeEvent::WorkflowCall through the same delegate bridge agent() uses (#12).',
    },
    {
      id: 'P2-workflow-depth-guard', headings: ['P2-workflow-depth-guard'],
      scope: 'the workflow() host handler from the prior ticket + core/src/agent/registry.rs depth helpers if needed; tests',
      extra: 'Enforce one-level nesting via exceeds_thread_spawn_depth_limit / next_thread_spawn_depth (registry). A workflow() call nested deeper than one level must throw (Rejected). Test: one level ok; two levels throws.',
    },
    {
      id: 'P2-test-workflow-nesting-uat10', headings: ['P2-test-workflow-nesting-uat10'],
      scope: 'workflow()-nesting unit tests + a hermetic UAT-10 in codex-rs/core/tests/suite/workflow_uat.rs',
      extra: 'UAT-10 asserts one-level workflow() nesting works and deeper nesting is rejected, on the hermetic fixture lane. Assert engine artifacts.',
    },
  ],
]

const buildPrompt = (t, priorInChain) => `You are implementing one ticket of Dynamic Workflows milestone M2 (Scheduling & governance, GitHub epic #22) in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). M0 + M1 are committed. GROUND TRUTH to build on (read to confirm; do not re-derive):
- Workflow globals gate on an explicit RuntimeState.workflow bool (threaded ExecuteRequest->RuntimeConfig); phase()/log()/agent()/parallel()/args/workflow.runId already install in the `if workflow {` block of code-mode/src/runtime/globals.rs.
- agent() flows RuntimeEvent::AgentCall -> cell_actor -> host delegate -> AgentControl::spawn_and_await_final_message, resolved by id; the bridge carries AgentSpawnOutcome{Completed(Value),Failed,Rejected(String)} where Rejected -> a JS throw (used already for AgentCapReached in CoreTurnHost::spawn_agent, core/src/tools/code_mode/delegate.rs).
- WorkflowScheduler (core/src/tools/code_mode/scheduler.rs) is live in the spawn path (concurrency permit + lifetime CAP). RolloutBudget (core/src/rollout_budget.rs) already has pub spent()/remaining() and a resettable configure cell (committed wave 1).
- The hermetic UAT harness lives in core/tests/suite/workflow_uat.rs (from #21) — reuse its mock-model + in-process-host pattern (disable Feature::CodeModeHost for the in-process lane).

Ticket: ${t.id}.${priorInChain ? `\n\nThis ticket CONTINUES a chain: the previous ticket "${priorInChain.ticket}" was just implemented in this working tree (files: ${JSON.stringify(priorInChain.files_changed)}; summary: ${priorInChain.summary.slice(0, 500)}). Build on it; never revert it.` : ''}

READ FIRST: (1) ${REPO}/docs/dynamic-workflows-plan.md section ${t.headings.map((h) => '"#### `' + h + '`"').join(' and ')} — follow its description + "_Acceptance:_" exactly; (2) the spec sections it references in ${REPO}/docs/dynamic-workflows-spec.md (§4/§5/§8 for budget+pipeline, §4/§6 for workflow()); (3) the named code precedents before writing code.

TICKET-SPECIFIC: ${t.extra}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command. RUST_MIN_STACK=8388608 for codex-core integration binaries. App-server test target is 'all'.
- Only modify files within this scope: ${t.scope}. Other agents edit OTHER areas of this shared working tree concurrently — never touch their files, never revert their changes, NEVER run git commit/add/push/stash/checkout. If a file you need is being co-edited (e.g. globals.rs), use targeted Edits to your own distinct region and let the verify pass reconcile transient churn.
- Never introduce wall-clock timeouts/sleeps into product code; no Date/Math/rand on the workflow path.
- Add the tests the acceptance criteria name; iterate targeted 'cargo test -p <package>' until green; then scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. cargo may block on the target-dir lock — wait it out.
- Do NOT change product behavior just to make a test pass; if a test reveals a real bug, note it in "notes". If blocked, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Implement')
log('Fanning out 3 M2 chains: [pipeline], [budget], [workflow-nesting]')
const chainResults = await parallel(
  CHAINS.map((chain) => async () => {
    const out = []
    let prior = null
    for (const t of chain) {
      const r = await agent(buildPrompt(t, prior), { label: t.id, phase: 'Implement', schema: RESULT, model: 'opus' })
      if (!r) { out.push({ ticket: t.id, status: 'blocked', files_changed: [], test_commands: [], tests_passed: false, summary: 'agent died/skipped', notes: '' }); break }
      out.push(r)
      if (r.status === 'blocked') break
      prior = r
    }
    return out
  })
)
const done = chainResults.filter(Boolean).flat()
log('M2 implement done: ' + done.filter((r) => r.status === 'done').length + '/' + done.length + ' tickets')

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), M2 tickets were just implemented as uncommitted working-tree changes on top of committed M0+M1:\n\n${JSON.stringify(done.map((r) => ({ ticket: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRECONCILE any concurrent-edit churn (esp. in code-mode/src/runtime/globals.rs, touched by pipeline + budget + workflow globals) so everything links, then FIX any breakage (compile errors, fmt, clippy, failing tests) WITHOUT changing intended behavior:\n1. cargo check --workspace (catch cross-crate exhaustive-match breaks from any new RuntimeEvent/notification variant)\n2. cargo fmt; 'cargo fmt --check' passes\n3. cargo clippy --all-targets for codex-core, codex-code-mode, codex-code-mode-protocol\n4. cargo test -p codex-code-mode; targeted codex-core tests (rollout_budget, workflow_uat incl. UAT-5/7/10, delegate, scheduler; RUST_MIN_STACK=8388608)\n5. Confirm intended behavior: pipeline() is no-barrier; agent() throws BudgetExceeded at the ceiling; workflow() nests one level and rejects deeper.\nNEVER git commit/add/push. Return structured result, ticket "m2-verify"; tests_passed=true only if all green (modulo the pre-existing rollout_budget stack overflow in the full core suite).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
