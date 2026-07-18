export const meta = {
  name: 'dw-wave1-foundations',
  description: 'Implement the dependency-free wave-1 tickets of the Dynamic Workflows plan in parallel (M0 spine leaves + M2/M3 leaves)',
  phases: [
    { title: 'Implement', detail: 'one agent per dependency-free ticket' },
    { title: 'Verify', detail: 'targeted fmt/clippy/test sweep over touched crates' },
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

const TICKETS = [
  {
    id: 'P0-feature-flag',
    issue: 2,
    headings: ['P0-feature-flag'],
    scope: 'the codex-rs/features crate only (src/lib.rs, src/feature_configs.rs, src/tests.rs)',
    extra: 'This ticket only DECLARES the flag + WorkflowConfigToml wiring inside the features crate; transitive dependency enforcement is a separate later ticket (#3) — do not implement it.',
  },
  {
    id: 'P0-meta-parser',
    issue: 4,
    headings: ['P0-meta-parser'],
    scope: 'the codex-rs/code-mode-protocol crate only (a new module or description.rs, plus unit tests)',
    extra: 'The parser must never construct or run a V8 isolate and must reject any non-literal meta. Preserve phases declaration order. Model it on parse_exec_source in code-mode-protocol/src/description.rs.',
  },
  {
    id: 'P2-budget-getters + P2-budget-resettable-cell',
    issue: 22,
    headings: ['P2-budget-getters', 'P2-budget-resettable-cell'],
    scope: 'codex-rs/core/src/rollout_budget.rs, codex-rs/core/src/agent/control.rs, and any direct users of RolloutBudget::configure that the OnceLock replacement forces you to touch (plus their tests)',
    extra: 'Implement BOTH plan tickets, getters first, then the resettable cell that replaces the OnceLock in the configure path. Keep the two changes logically separable in your summary (they will be committed as two commits). Do not change budget accounting semantics — only add getters and make the cell resettable.',
  },
  {
    id: 'P3-journal-crate-types',
    issue: 23,
    headings: ['P3-journal-crate-types'],
    scope: 'a NEW crate directory under codex-rs/ (name it per the plan — codex-workflow-journal package; follow neighbor crates for directory naming) plus the single workspace-members addition in codex-rs/Cargo.toml',
    extra: 'Types only per the plan ticket: JournalLine envelope + WorkflowRunMeta and associated serde round-trip tests. No recorder, no replay, no key hashing — those are later tickets.',
  },
]

const buildPrompt = (t) => `You are implementing one ticket of the Dynamic Workflows feature in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl is already checked out; the Rust workspace root is ${REPO}/codex-rs).

Ticket: ${t.id} — tracked under GitHub issue #${t.issue}.

READ FIRST, in order:
1. ${REPO}/docs/dynamic-workflows-plan.md — locate the ticket detail section(s) headed ${t.headings.map((h) => '"#### `' + h + '`"').join(' and ')} and follow the description and the "_Acceptance:_" criteria exactly.
2. The spec sections referenced by the ticket in ${REPO}/docs/dynamic-workflows-spec.md.
3. The existing code precedents the ticket names (file:line references) — study them before writing any code so your change matches the established pattern.

RULES:
- Toolchain: run 'export PATH="$HOME/.cargo/bin:$PATH"' in every shell command before cargo (Rust 1.95.0 via rustup, already installed).
- Only create/modify files within this scope: ${t.scope}. Do NOT touch other tickets' areas. NEVER run git commit/add/push/stash — leave everything as working-tree changes.
- ${t.extra}
- Add the unit tests the acceptance criteria name and iterate until green (targeted 'cargo test -p <package>' runs; resolve package names from each crate's Cargo.toml).
- Then run scoped 'cargo fmt' and 'cargo clippy -p <package> --all-targets' and fix anything your change introduced.
- Other agents are concurrently editing OTHER crates in this same working tree; cargo may block on the target-dir lock — just wait it out. Ignore compile errors originating in files outside your scope.
- If genuinely blocked, return status "blocked" and explain in notes.

Return the structured result. files_changed must list every file you created or modified as repo-relative paths.`

phase('Implement')
log('Fanning out ' + TICKETS.length + ' implementation agents over the dependency-free wave-1 tickets')
const results = await parallel(
  TICKETS.map((t) => () =>
    agent(buildPrompt(t), { label: t.id, phase: 'Implement', schema: RESULT, model: 'opus' })
  )
)

const done = results.filter(Boolean)
log('Implementation done: ' + done.filter((r) => r.status === 'done').length + '/' + TICKETS.length + ' tickets report done')

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (Rust workspace ${REPO}/codex-rs; run 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command), the following wave-1 Dynamic Workflows tickets were just implemented as uncommitted working-tree changes:\n\n${JSON.stringify(done.map((r) => ({ ticket: r.ticket, status: r.status, files: r.files_changed, summary: r.summary })), null, 2)}\n\nRun this verification sweep and FIX any breakage you find (compile errors, fmt diffs, clippy warnings, failing tests) without changing intended behavior:\n1. cargo fmt (workspace) and confirm 'cargo fmt --check' passes\n2. cargo clippy --all-targets for: the features crate, the code-mode-protocol crate, and the new workflow-journal crate (resolve -p package names from their Cargo.tomls)\n3. cargo test for those same three crates\n4. cargo test -p codex-core rollout_budget (targeted budget tests)\n5. cargo check -p codex-core (compile-only sanity for the core changes)\nNEVER run git commit/add/push. Return the structured result with ticket "wave1-verify"; tests_passed=true only if every step above ended green; list in files_changed only files YOU changed during fixes.`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
