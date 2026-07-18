export const meta = {
  name: 'dw-wave3-m1-core',
  description: 'Implement the M1 orchestration core of the Dynamic Workflows plan (#9 #10 #12 #13 #14 #15 #16 #17 #18 #19 #20) in two dependency stages',
  phases: [
    { title: 'Stage 1', detail: 'chains: [#9→#10], [#13→#14→#15], [#20]' },
    { title: 'Stage 2', detail: 'chains: [#12→#16], [#17], [#18→#19]' },
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

const STAGE1 = [
  [
    {
      id: 'P1-agentcall-runtime-types', issue: 9, headings: ['P1-agentcall-runtime-types'],
      scope: 'codex-rs/code-mode/src/runtime/mod.rs (RuntimeEvent::AgentCall + AgentCallOpts + RuntimeState.next_agent_ordinal) and any protocol-crate type home the existing RuntimeEvent variants use',
      extra: 'Pure type surface — no dispatch/callback behavior. Note RuntimeEvent::Phase/WorkflowLog were added by wave 2; follow their placement. AgentCallOpts: label/phase/schema/model/effort/isolation/agentType all optional, unknown fields ignored (NOT deny_unknown_fields). Round-trip unit test required.',
    },
    {
      id: 'P1-agent-callback', issue: 10, headings: ['P1-agent-callback'],
      scope: 'codex-rs/code-mode/src/runtime/callbacks.rs and globals.rs (agent global, workflow-gated like phase/log from wave 2)',
      extra: 'Model exactly on tool_callback: mint PromiseResolver, stamp ordinal = state.next_agent_ordinal++ SYNCHRONOUSLY before returning the promise, store resolver in pending_tool_calls under a fresh id, emit RuntimeEvent::AgentCall. No host spawn wiring (that is #12). The isolate test must prove Promise.all([agent(a),agent(b),agent(c)]) yields ordinals 0,1,2 in source order. GATING: wave 2 replaced source-sniffing with an EXPLICIT flag — install the agent global ONLY when RuntimeState.workflow is true (the same workflow: bool that gates phase()/log(), threaded from ExecuteRequest through RuntimeConfig). Follow exactly how globals.rs installs phase/log under that flag today.',
    },
  ],
  [
    {
      id: 'P1-opts-model-effort', issue: 13, headings: ['P1-opts-model-effort'],
      scope: 'codex-rs/core/src/agent/control/spawn_await.rs (+ its tests), core/src/agent/control.rs SpawnAgentOptions if a field is needed, and a small opts module next to spawn_await if needed',
      extra: 'The committed helper is spawn_and_await_final_message(.., options: SpawnAgentOptions) and builds the child config via build_agent_spawn_config before spawning. Apply opts.model + opts.effort in that config-build step: opts.model via apply_requested_spawn_agent_model_overrides, effort mapping low..max -> ReasoningEffort validated against the resolved model supported_reasoning_levels; omitted values inherit parent config. Thread the requested model/effort in via SpawnAgentOptions (or a dedicated request struct) — decide and document. This and the next two tickets all edit spawn_await.rs — you own the file for this chain, apply them sequentially.',
    },
    {
      id: 'P1-opts-agenttype', issue: 14, headings: ['P1-opts-agenttype'],
      scope: 'codex-rs/core/src/agent/control/spawn_await.rs (+ tests)',
      extra: 'opts.agentType -> apply_role_to_config, DEFAULT_ROLE_NAME fallback, unknown role errors (not silent default), applied in the documented spawn_agent order relative to model/effort.',
    },
    {
      id: 'P1-deterministic-nickname', issue: 15, headings: ['P1-deterministic-nickname'],
      scope: 'codex-rs/core/src/agent/control/spawn_await.rs (+ tests) and read-only study of registry.rs reserve_agent_nickname_with_preference',
      extra: 'Derive the preferred nickname purely from the invocation ordinal/index and pass it through the spawn path so no rand::rng() is reachable for workflow agents; collision fallback must be deterministic. Two identical fan-outs must assign identical nicknames per ordinal.',
    },
  ],
  [
    {
      id: 'P1-args-injection', issue: 20, headings: ['P1-args-injection'],
      scope: 'codex-rs/code-mode/src/runtime/globals.rs (+ mod.rs plumbing), code-mode-protocol ExecuteRequest/RuntimeConfig (to carry args + runId, alongside the workflow flag added in wave 2), and codex-rs/core/src/tools/code_mode/workflow_handler.rs (set args + runId)',
      extra: 'Inject invocation JSON read-only as global args via json_to_v8 (build_tools_object precedent); mint runId host-side in Rust with uuid v7 (never in JS) exposed read-only as workflow.runId. Thread args + runId from the workflow handler through ExecuteRequest -> RuntimeConfig the SAME way wave 2 threaded the workflow: bool flag (follow that exact plumbing, incl. the CreateCellRequest round-trip and wire snapshot). Install args/workflow.runId ONLY when RuntimeState.workflow is true. Assignment to either must throw or be ignored (tested).',
    },
  ],
]

const STAGE2 = [
  [
    {
      id: 'P1-cellactor-spawn-dispatch', issue: 12, headings: ['P1-cellactor-spawn-dispatch'],
      scope: 'codex-rs/code-mode/src/cell_actor/mod.rs, codex-rs/core/src/tools/code_mode/delegate.rs (DispatchMessage::SpawnAgent), workflow_handler.rs wiring, and integration tests',
      extra: 'Route RuntimeEvent::AgentCall (from #10, now in the tree) to the wave-2 spawn helper. Its REAL committed signature is AgentControl::spawn_and_await_final_message(&self, base_instructions: &BaseInstructions, parent_turn: &TurnContext, parent_thread_id: ThreadId, input: Vec<UserInput>, options: SpawnAgentOptions) -> Option<String> (in core/src/agent/control/spawn_await.rs) — it constructs the Subagent source itself and returns Some(text)/None. The cell_actor/delegate live in code-mode and cannot depend on codex-core directly, so plumb the call through the host delegate that the workflow_handler already holds (the AgentControl + parent turn/thread context live on the core side, same place the workflow handler runs): add DispatchMessage::SpawnAgent carrying {prompt, opts}, handle it by spawning one independent tokio task in the existing JoinSet that invokes the helper, and feed the answer back as RuntimeCommand::ToolResponse{id,result} so resolve_tool_response resolves by id. String -> JS string; None -> JS null (never throw for agent failure). Study how the existing tool_callback dispatch bridges code-mode<->core (delegate boundary) and mirror it. Fixture-model integration test: await agent("p") resolves to child final text; dead agent -> null; 16 concurrent calls resolve independently out-of-order without serialization.',
    },
    {
      id: 'P1-opts-schema', issue: 16, headings: ['P1-opts-schema'],
      scope: 'codex-rs/core/src/agent/control/spawn_await.rs, the #12 dispatch path files, and tests',
      extra: 'Thread opts.schema -> final_output_json_schema on the child (build_prompt sets output_schema_strict). On return serde_json parse + jsonschema-crate revalidation (defense-in-depth); parse/validation failure resolves to null; without schema return plain text. Marshal parsed object to JS via json_to_v8.',
    },
  ],
  [
    {
      id: 'P1-parallel-prelude', issue: 17, headings: ['P1-parallel-prelude'],
      scope: 'the injected JS prelude in codex-rs/code-mode/src/runtime/ (wherever the workflow prelude lives after #10) + in-isolate tests',
      extra: 'parallel(thunks) = Promise.all(thunks.map(t => t().catch(() => null))), position-preserving barrier, thunks.length <= 4096 validated BEFORE dispatch with a descriptive error. Pure prelude — no host op.',
    },
  ],
  [
    {
      id: 'P1-scheduler-semaphore', issue: 18, headings: ['P1-scheduler-semaphore'],
      scope: 'a new WorkflowScheduler module in codex-rs/core/src/tools/code_mode/ (or core/src/agent/) + wiring into the #12 dispatch path + tests',
      extra: 'tokio Semaphore cap = min(16, available_parallelism-2) clamped via normalize_concurrency with the workflow-raised effective_agent_max_threads per the spec §5 cap-override note. Permit acquired before spawn, dropped on finalize (success AND failure paths). reserve_spawn_slot stays the hard backstop; requeue on AgentLimitReached. Instrumented stalled-children fixture test proving in-flight never exceeds cap and a 32-way parallel with cap 8 completes.',
    },
    {
      id: 'P1-lifetime-cap', issue: 19, headings: ['P1-lifetime-cap'],
      scope: 'the WorkflowScheduler module from #18 (+ tests)',
      extra: 'AtomicUsize lifetime_spawned, ceiling 1000, CAS-incremented at admission BEFORE the concurrency permit (admission-order step 2 before step 4), never decrements, per-run (fresh scheduler = fresh count). 1001st admission throws AgentCapReached.',
    },
  ],
]

const buildPrompt = (t, priorInChain) => `You are implementing one ticket of the Dynamic Workflows feature in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; Rust workspace ${REPO}/codex-rs). Waves 1-2 are committed. GROUND TRUTH you must build on (verify by reading, do not re-derive):
- Workflow-ness is an EXPLICIT flag, not source-sniffing: a serde-defaulted 'workflow: bool' threads ExecuteRequest -> WireExecuteRequest -> CreateCellRequest -> RuntimeConfig -> RuntimeState.workflow; it is omitted on the wire when false. phase()/log() globals install only when that flag is true. Any new workflow global (agent/args/budget/workflow.runId) gates the SAME way and, if it needs host->isolate data, threads through the SAME plumbing.
- The spawn keystone is AgentControl::spawn_and_await_final_message(&self, base_instructions: &BaseInstructions, parent_turn: &TurnContext, parent_thread_id: ThreadId, input: Vec<UserInput>, options: SpawnAgentOptions) -> Option<String> in core/src/agent/control/spawn_await.rs. It spawns via the registering deferred path, subscribes to a non-competing broadcast event tap (Session::subscribe_events), filters terminal events by submission id, and returns Some(final_text)/None. Do NOT reintroduce next_event() draining or run_codex_thread_one_shot.
- Committed too: Feature::Workflow + transitive deps, meta parser, budget getters/resettable cell, workflow-journal types, codex-core-workflows loader, workflows watcher, CodeModeWorkflowHandler.

Ticket: ${t.id} — GitHub issue #${t.issue}.${priorInChain ? `\n\nThis ticket CONTINUES a chain: previous ticket "${priorInChain.ticket}" was just implemented in this working tree (files: ${JSON.stringify(priorInChain.files_changed)}; summary: ${priorInChain.summary.slice(0, 500)}). Build on it; never revert it.` : ''}

READ FIRST: (1) ${REPO}/docs/dynamic-workflows-plan.md section(s) ${t.headings.map((h) => '"#### `' + h + '`"').join(' and ')} — follow the description and "_Acceptance:_" criteria exactly; (2) the spec sections it references in ${REPO}/docs/dynamic-workflows-spec.md; (3) the named code precedents (file:line) before writing code.

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only create/modify files within this scope: ${t.scope}. Other agents are concurrently editing OTHER areas — never touch their files, never revert working-tree changes you did not write, NEVER run git commit/add/push/stash/checkout.
- ${t.extra}
- Add the tests the acceptance criteria name; iterate targeted 'cargo test -p <package>' until green; then scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. RUST_MIN_STACK=8388608 works around the pre-existing codex-core tests/all.rs rollout_budget stack overflow. The app-server integration test target is 'all' (cargo test -p codex-app-server --test all <filter>).
- Cargo may block on the shared target-dir lock — wait it out. If genuinely blocked, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

const runChains = (chains, phaseName) =>
  parallel(
    chains.map((chain) => async () => {
      const out = []
      let prior = null
      for (const t of chain) {
        const r = await agent(buildPrompt(t, prior), { label: t.id, phase: phaseName, schema: RESULT, model: 'opus' })
        if (!r) { out.push({ ticket: t.id, status: 'blocked', files_changed: [], test_commands: [], tests_passed: false, summary: 'agent died/skipped', notes: '' }); break }
        out.push(r)
        if (r.status === 'blocked') break
        prior = r
      }
      return out
    })
  )

phase('Stage 1')
log('Stage 1: [#9→#10], [#13→#14→#15], [#20]')
const stage1 = (await runChains(STAGE1, 'Stage 1')).filter(Boolean).flat()
log('Stage 1 done: ' + stage1.filter((r) => r.status === 'done').length + '/6 tickets')

phase('Stage 2')
log('Stage 2: [#12→#16], [#17], [#18→#19]')
const stage2 = (await runChains(STAGE2, 'Stage 2')).filter(Boolean).flat()
log('Stage 2 done: ' + stage2.filter((r) => r.status === 'done').length + '/5 tickets')

const done = [...stage1, ...stage2]

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), wave-3 M1 tickets were just implemented as uncommitted working-tree changes on top of committed waves 1-2:\n\n${JSON.stringify(done.map((r) => ({ ticket: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRun and FIX any breakage (without changing intended behavior):\n1. cargo fmt; 'cargo fmt --check' passes\n2. cargo clippy --all-targets for codex-code-mode and codex-core\n3. cargo test -p codex-code-mode (full — no regression to the 85+ existing tests)\n4. Targeted codex-core tests each ticket added (spawn_await, workflow handler, dispatch, scheduler; RUST_MIN_STACK=8388608 as needed)\n5. cargo check -p codex-core -p codex-app-server\nNEVER git commit/add/push. Return structured result, ticket "wave3-verify"; tests_passed=true only if all green (modulo the documented pre-existing rollout_budget stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
