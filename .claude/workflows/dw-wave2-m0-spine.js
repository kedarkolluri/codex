export const meta = {
  name: 'dw-wave2-m0-spine',
  description: 'Implement the M0 spine of the Dynamic Workflows plan (#3 #5 #6 #7 #8) plus the P1 spawn-await helper (#11) as four parallel chains',
  phases: [
    { title: 'Implement', detail: 'four parallel chains: [#3], [#5→#6], [#7→#8], [#11]' },
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

// Four independent chains; tickets inside a chain run sequentially because the
// later ticket builds directly on the earlier one's code.
const CHAINS = [
  [
    {
      id: 'P0-feature-transitive-deps',
      issue: 3,
      headings: ['P0-feature-transitive-deps'],
      scope: 'codex-rs/features (normalize_dependencies + tests) and codex-rs/core/src/config/mod.rs (explicit-conflict validation near the existing MultiAgentV2 checks)',
      extra: 'Feature::Workflow, WorkflowConfigToml, and the workflow key already landed (ticket #2, committed). Recommended semantics per the plan: normalize_dependencies silently auto-enables CodeMode + MultiAgentV2 when unset; config resolution errors ONLY when a dependency is explicitly disabled while Workflow is on — the error message must name the missing dependency and how to fix it.',
    },
  ],
  [
    {
      id: 'P0-core-workflows-loader',
      issue: 5,
      headings: ['P0-core-workflows-loader'],
      scope: 'a NEW codex-rs/core-workflows crate (clone the core-skills loader structure), the workspace-members line in codex-rs/Cargo.toml, and minimal wiring into config/session construction where core-skills roots are assembled',
      extra: 'parse_workflow_meta already exists in codex-code-mode-protocol (ticket #4, committed) — depend on that crate and use it; never evaluate a workflow body. Fail-open on parse errors like load_skill_metadata (core-skills/src/loader.rs:760). Precedence: <repo>/.codex/workflows > $HOME/.agents/workflows > $CODEX_HOME/workflows.',
    },
    {
      id: 'P0-workflows-watcher',
      issue: 6,
      headings: ['P0-workflows-watcher'],
      scope: 'codex-rs/app-server (new workflows_watcher.rs + registration), codex-rs/app-server-protocol/src/protocol/common.rs (WorkflowsChanged variant + payload struct)',
      extra: 'Builds directly on the core-workflows loader you just implemented in this same session. Clone skills_watcher.rs; map the notification to wire string "workflows/changed" next to SkillsChanged; export TS/JSON schema the same way.',
    },
  ],
  [
    {
      id: 'P0-host-tool-skeleton',
      issue: 7,
      headings: ['P0-host-tool-skeleton'],
      scope: 'codex-rs/core/src/tools/code_mode/ (new workflow handler cloned from execute_handler.rs + registration behind Feature::Workflow), plus any minimal core tool-registry wiring it needs',
      extra: 'Feature::Workflow (ticket #2) and parse_workflow_meta (ticket #4) are committed — use them. Run the body ONCE in a FRESH isolate via the existing code_mode_service.execute / run_runtime path. NO agent()/journal/budget/worktree/determinism yet. Must not regress existing code-mode behavior — share the service, do not fork the runtime; all existing code-mode tests must stay green.',
    },
    {
      id: 'P0-phase-log-globals',
      issue: 8,
      headings: ['P0-phase-log-globals'],
      scope: 'codex-rs/code-mode/src/runtime/ (globals.rs, mod.rs, callbacks.rs as needed) and the workflow handler you just built (to surface the events)',
      extra: 'Builds directly on the host-tool skeleton you just implemented in this same session. Emit RuntimeEvent::Phase / RuntimeEvent::WorkflowLog only — the protocol EventMsg cluster and journaling are later tickets. The new globals must be installed ONLY for workflow runs, not plain code-mode exec sessions.',
    },
  ],
  [
    {
      id: 'P1-spawn-await-helper',
      issue: 11,
      headings: ['P1-spawn-await-helper'],
      scope: 'codex-rs/core/src/agent/control/ (new helper module) and its integration test (fixture model via create_mock_responses_server_sequence)',
      extra: 'This is the L-sized keystone of M1. Spawn through the REGISTERING path (spawn_agent_with_communication -> spawn_agent_internal -> spawn_new_thread_with_source(ThreadSource::Subagent)) with build_agent_spawn_config; consume the child event stream to TurnComplete/TurnAborted mirroring tasks/review.rs::process_review_events; return Some(last_agent_message) / None. NEVER use run_codex_thread_one_shot or Codex::spawn directly. Signature must be call-site-agnostic (future wait_agent could share it). The integration test must assert notify_thread_created fires, a spawn edge is written to agent-graph-store, the child rollout file exists, and get_thread(child_id) succeeds.',
    },
  ],
]

const buildPrompt = (t, priorInChain) => `You are implementing one ticket of the Dynamic Workflows feature in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl checked out; Rust workspace root ${REPO}/codex-rs). Wave-1 tickets (#2 feature flag, #4 meta parser, budget getters/resettable cell, workflow-journal types) are already committed on this branch.

Ticket: ${t.id} — tracked under GitHub issue #${t.issue}.${priorInChain ? `\n\nThis ticket CONTINUES a chain: the previous ticket "${priorInChain.ticket}" was just implemented in this working tree (files: ${JSON.stringify(priorInChain.files_changed)}; summary: ${priorInChain.summary.slice(0, 600)}). Build on it; do not re-do or revert it.` : ''}

READ FIRST, in order:
1. ${REPO}/docs/dynamic-workflows-plan.md — locate the ticket detail section(s) headed ${t.headings.map((h) => '"#### `' + h + '`"').join(' and ')} and follow the description and the "_Acceptance:_" criteria exactly.
2. The spec sections referenced by the ticket in ${REPO}/docs/dynamic-workflows-spec.md.
3. The existing code precedents the ticket names (file:line references) — study them before writing any code so your change matches the established pattern.

RULES:
- Toolchain: run 'export PATH="$HOME/.cargo/bin:$PATH"' in every shell command before cargo (Rust 1.95.0 via rustup).
- Only create/modify files within this scope: ${t.scope}. Other agents are concurrently editing OTHER areas of this same working tree — do NOT touch their files, do NOT revert anything you did not write, and NEVER run git commit/add/push/stash/checkout. Leave everything as working-tree changes.
- ${t.extra}
- Add the tests the acceptance criteria name and iterate until green (targeted 'cargo test -p <package>' runs; resolve package names from Cargo.toml). Known pre-existing issue: the codex-core tests/all.rs integration binary can hit a stack overflow in suite::rollout_budget under the default 2MB test stack — that is environmental, not yours; use RUST_MIN_STACK=8388608 if you hit it.
- Then run scoped 'cargo fmt' and 'cargo clippy -p <package> --all-targets' and fix anything your change introduced.
- Cargo may block on the target-dir lock while other agents build — wait it out. Ignore compile errors originating outside your scope.
- If genuinely blocked, return status "blocked" and explain in notes.

Return the structured result. files_changed must list every file you created or modified as repo-relative paths.`

phase('Implement')
log('Fanning out 4 parallel chains: [#3], [#5→#6], [#7→#8], [#11]')

const chainResults = await parallel(
  CHAINS.map((chain) => async () => {
    const out = []
    let prior = null
    for (const t of chain) {
      const r = await agent(buildPrompt(t, prior), {
        label: t.id,
        phase: 'Implement',
        schema: RESULT,
        model: 'opus',
      })
      if (!r) {
        out.push({ ticket: t.id, status: 'blocked', files_changed: [], test_commands: [], tests_passed: false, summary: 'agent died/skipped', notes: '' })
        break
      }
      out.push(r)
      if (r.status === 'blocked') break
      prior = r
    }
    return out
  })
)

const done = chainResults.filter(Boolean).flat()
log('Implementation done: ' + done.filter((r) => r.status === 'done').length + '/6 tickets report done')

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (Rust workspace ${REPO}/codex-rs; run 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command), the following wave-2 Dynamic Workflows tickets were just implemented as uncommitted working-tree changes on top of the committed wave-1 work:\n\n${JSON.stringify(done.map((r) => ({ ticket: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 500) })), null, 2)}\n\nRun this verification sweep and FIX any breakage you find (compile errors, fmt diffs, clippy warnings, failing tests) without changing intended behavior:\n1. cargo fmt (workspace) and confirm 'cargo fmt --check' passes\n2. cargo clippy --all-targets for every crate the tickets touched (resolve -p names from Cargo.toml: features, core-workflows, app-server, app-server-protocol, code-mode, and the code_mode tool area of codex-core)\n3. cargo test for: codex-features, the new core-workflows crate, codex-code-mode, and the targeted new tests each ticket added (use RUST_MIN_STACK=8388608 for codex-core integration binaries; the pre-existing suite::rollout_budget stack overflow under the default stack is NOT yours to fix)\n4. cargo check -p codex-core -p codex-app-server\n5. Confirm existing code-mode tests are green (no regression from the workflow handler / new globals)\nNEVER run git commit/add/push. Return the structured result with ticket "wave2-verify"; tests_passed=true only if every step ended green (modulo the documented pre-existing stack overflow); list in files_changed only files YOU changed during fixes.`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
