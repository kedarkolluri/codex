export const meta = {
  name: 'dw-wave3-fixes',
  description: 'Wire the wave-3 M1 agent() path into production: remote-host spawn bridge + error channel, scheduler/lifetime/nickname wiring, input/schema bounds, cleanup, default-role, args/runId wire-compat',
  phases: [
    { title: 'Fix', detail: 'two coupled agents across the crate seam + independent bazel/test fixes' },
    { title: 'Verify', detail: 'reconcile the seam, fmt/clippy/test sweep' },
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

// The SHARED SEAM both agents build against. Agent B owns the type definition +
// cell_actor mapping; Agent A consumes it in CoreTurnHost::spawn_agent.
const SEAM = `SHARED SEAM CONTRACT (both fix agents must implement EXACTLY this so the two sides interlock):
Replace the spawn bridge's Option<JsonValue> with a three-way outcome so the host can RESOLVE, resolve-to-null, or REJECT (throw) the JS promise:

  // code-mode-protocol/src/session.rs
  #[derive(Debug, Clone)]
  pub enum AgentSpawnOutcome {
      Completed(serde_json::Value), // resolve the agent() promise with this value (string or validated object)
      Failed,                       // resolve the agent() promise to JS null (agent died/aborted/parse-fail) — death-is-null
      Rejected(String),             // REJECT/throw in JS with this message (e.g. "AgentCapReached", "BudgetExceeded")
  }
  pub type AgentSpawnFuture = Pin<Box<dyn Future<Output = AgentSpawnOutcome> + Send + 'static>>;

The DispatchMessage::SpawnAgent response oneshot carries AgentSpawnOutcome (not Option<JsonValue>).
cell_actor dispatch (code-mode/src/cell_actor/callbacks.rs) maps: Completed(v) -> RuntimeCommand::ToolResponse{id,result:v}; Failed -> ToolResponse{id, result: JsonValue::Null}; Rejected(msg) -> the isolate's ERROR/throw path so the JS promise REJECTS with msg (mirror how tool_callback surfaces a tool error today — find that path and reuse it).
The CoreTurnHost::spawn_agent impl (core/src/tools/code_mode/delegate.rs) returns Completed/Failed/Rejected accordingly: a normal final message -> Completed; agent death/abort/schema-parse-null -> Failed; a scheduler lifetime-cap/budget rejection at admission -> Rejected("AgentCapReached"/...).`

const FIXES = [
  {
    id: 'fix-w3-core-wiring',
    label: 'core-side production wiring',
    scope: 'codex-rs/core/src/tools/code_mode/delegate.rs, scheduler.rs, scheduler_tests.rs, mod.rs; codex-rs/core/src/agent/control/spawn_await.rs, spawn_await_opts.rs, spawn_await_tests.rs, control.rs; codex-rs/core/src/agent/registry.rs; and the multi_agents*/spawn.rs preferred_agent_nickname sites if needed',
    findings: `You own the CORE side of the agent() spawn path. Consume the shared seam type AgentSpawnOutcome (Agent B defines it in code-mode-protocol/src/session.rs and updates the cell_actor mapping; if it is not present yet when you compile, define a matching local expectation and let the verify pass reconcile — do NOT edit session.rs/cell_actor, those are Agent B's).

Fix these VERIFIED findings:
1. BLOCKER (scheduler dead code, delegate.rs / scheduler.rs): WorkflowScheduler + the lifetime cap are never called from the live dispatch — CoreTurnHost::spawn_agent (delegate.rs ~398-437) spawns directly. WIRE the scheduler into the production spawn path following spec §5 admission order: (step 2) lifetime CAS to LIFETIME_SPAWN_CAP (1000) BEFORE the concurrency permit — over-limit => return AgentSpawnOutcome::Rejected("AgentCapReached"); (step 4) await a concurrency permit from the WorkflowScheduler semaphore before spawning; (step 6) DROP the permit on finalize on ALL paths (success, death, abort, error, panic/cancel — use a guard/RAII so a panic or dropped future still releases). One WorkflowScheduler instance must be shared per workflow run (construct it where the workflow's CoreTurnHost/broker is created, not per-call). Budget pre-admission (step 1) is M2 — leave a clearly-marked TODO hook but do not implement budget here.
2. BLOCKER (nickname ordinal discarded, delegate.rs ~404 'let _ = ordinal'): set SpawnAgentOptions.preferred_agent_nickname from the invocation ordinal via the existing workflow_agent_nickname_preference(ordinal) mapper (spawn_await.rs ~428) on the PRODUCTION path so nicknames are a pure function of ordinal (no rand::rng()).
3. MAJOR (collision unresolved, registry.rs ~221): reserve_agent_nickname_with_preference returns the requested name without checking used_agent_nicknames and ignores the insert result. Make the preferred branch resolve collisions DETERMINISTICALLY (e.g. append -2, -3, … or a documented deterministic suffix) and actually reserve; never fall back to rand for a workflow-preferred name.
4. MAJOR (child leak, spawn_await.rs ~228 / delegate.rs ~150): if send_input(_with_schema) fails after the child is registered, or the spawn-and-await future is cancelled/dropped, the registered child is never terminated (leaks registry slot/nickname and, now, would leak a permit). Ensure a registered child is terminated/reaped on submit-failure and on cancellation (Drop guard or explicit cleanup on the error path).
5. MAJOR (default role skipped, spawn_await.rs ~87): overrides.apply() is skipped when all opts are absent, so apply_role_to_config(.., None) never runs and a user-defined role literally named "default" is not applied for a bare agent("p"). Ensure the role-resolution/default-role step runs even when model/effort/agentType are all absent (match the normal V2 spawn path's behavior).
6. BLOCKER (unbounded prompt, delegate.rs ~427): the V8 prompt becomes UserInput::Text with no cap. Enforce a hard byte ceiling on the incoming prompt before it becomes child context (reject/Reject or truncate with a marker — choose per the repo's 10K-token individual-context rule; a hard byte cap like 64 KiB is reasonable, document it).
7. BLOCKER (unbounded schema, delegate.rs ~407/431): opts.schema is copied into every child prompt and recompiled each return with no size/depth bound. Bound the incoming schema (serialized-byte cap + nesting-depth cap) BEFORE use; over-limit => AgentSpawnOutcome::Rejected or Failed with a clear reason.
8. Test coverage (findings 13/15/16): add scheduler_tests for permit release on cancellation/drop and on a panicking admission; keep the existing cap/lifetime tests. (The real-core end-to-end concurrency test belongs to Agent B's process-host bridge + the verify pass; you cover the scheduler/permit units here.)

Determinism: no Date/Math/rand on the workflow spawn path; nickname + collision fallback must be pure functions of the ordinal.`,
  },
  {
    id: 'fix-w3-host-bridge',
    label: 'host bridge + error channel',
    scope: 'codex-rs/code-mode-protocol/src/session.rs, host/message.rs, host/payload.rs, lib.rs; codex-rs/code-mode-host/src/delegate.rs, lib.rs; codex-rs/code-mode/src/cell_actor/callbacks.rs, cell_actor/types.rs, cell_actor/mod.rs; codex-rs/code-mode/src/service.rs, session_runtime/types.rs, session_runtime/mod.rs; and the process-host round-trip tests + code-mode/tests/agent_dispatch.rs',
    findings: `You own the SEAM + the remote/process host bridge. Implement the shared AgentSpawnOutcome contract (see the SEAM section) and the wire round-trip so a REAL (default-config, process-owned host) workflow can spawn agents.

Fix these VERIFIED findings:
1. BLOCKER (process host cannot spawn, code-mode-host/src/delegate.rs + host/message.rs): Feature::CodeModeHost is default-ON, so production workflows run in the process-owned host where RemoteDelegate has NO spawn_agent and inherits the default that returns None — every agent() resolves to null. ADD a spawn request/response variant to the host wire protocol (host/message.rs DelegateRequest::SpawnAgent{prompt, opts, ordinal} + DelegateResponse::AgentSpawned{outcome}), implement RemoteDelegate::spawn_agent (code-mode-host/src/delegate.rs) to serialize the request over the wire and await the response, and handle it on the host side (code-mode-host/src/lib.rs) by forwarding to the real in-process delegate/broker path so it reaches core's spawn. The outcome must round-trip AgentSpawnOutcome (Completed/Failed/Rejected) faithfully.
2. BLOCKER (bridge cannot reject, session.rs ~34 + cell_actor/callbacks.rs ~110): change AgentSpawnFuture / DispatchMessage::SpawnAgent to carry AgentSpawnOutcome per the SEAM. In cell_actor dispatch map Completed->ToolResponse(value), Failed->ToolResponse(null), Rejected(msg)->the isolate throw/reject path (find how tool errors surface to a JS promise today — the tool_callback error path — and reuse it) so the 1001st agent()/over-budget call THROWS AgentCapReached/BudgetExceeded in JS instead of resolving null.
3. MAJOR (args/run_id V1 break, host/payload.rs ~160): args + run_id were added to the strict deny_unknown_fields V1 ExecuteRequest without skip_serializing_if, so a new client's workflow request with args is rejected by an older host. Add #[serde(default, skip_serializing_if = "Option::is_none")] to args and run_id on both ExecuteRequest and WireExecuteRequest (matching how the wave-2 'workflow' bool is omitted when false) so a request without them is byte-shape-identical to legacy; only a real workflow-with-args request (a new capability) carries them. Update/keep the wire round-trip + snapshot tests.
4. Test coverage (findings 13/14): replace/augment the FixtureAgentDelegate-only tests in code-mode/tests/agent_dispatch.rs so at least one test drives the REAL process-host bridge end-to-end (16-way concurrency through the actual spawn_agent wire path resolving by id without serialization), and a combined test that forwards opts.schema through the bridge and asserts the final raw-text->validation->JS-object marshalling (not just the pre-parsed piece). If a real-core spawn is not reachable from the code-mode test crate, drive as much of the process-host wire round-trip as possible with a host-side stub that returns each AgentSpawnOutcome variant and assert the isolate sees value/null/throw respectively.

Coordinate with Agent A only through the SEAM type: you DEFINE AgentSpawnOutcome and AgentSpawnFuture in session.rs and the cell_actor mapping; Agent A consumes them in core. Do not edit core/src/tools/code_mode/delegate.rs or core/src/agent/** (Agent A owns those).`,
  },
]

const buildPrompt = (f) => `You are applying code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). The tree holds the wave-3 M1 orchestration core (plan: docs/dynamic-workflows-plan.md; spec: docs/dynamic-workflows-spec.md §5-§8). A codex review + an independent code audit CONFIRMED every finding below against the code — they are real. The overarching gap: the agent() spawn path was implemented but NOT wired into the default-config production path (the process-owned host can't spawn; the scheduler/lifetime cap are dead code; the nickname ordinal is discarded). Your job is to complete the wiring.

${SEAM}

Your fix bundle: ${f.id} (${f.label})

${f.findings}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only modify files within this scope: ${f.scope}. The OTHER fix agent is editing the other side of the seam concurrently — never edit their files, never revert their work; rely on the SEAM contract to interlock and the verify pass to reconcile any transient cross-file compile churn.
- NEVER run git commit/add/push/stash/checkout. Never introduce wall-clock timeouts/sleeps into product code. No Date/Math/rand on the workflow path.
- Add/extend the tests the findings name; iterate targeted 'cargo test -p <package>' until green (RUST_MIN_STACK=8388608 for codex-core; app-server target 'all'). Then scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean for your crates. If the crate won't compile because the OTHER agent's seam half isn't in yet, get YOUR side internally consistent, note it, and return done (the verify pass links both halves).
- Cargo may block on the shared target-dir lock — wait it out. If genuinely blocked on something you cannot resolve, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 2 coupled fix agents across the crate seam')
const results = await parallel(
  FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' }))
)
const done = results.filter(Boolean)
log('Fixes done: ' + done.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), two coupled fix agents just wired the wave-3 agent() path into production across a crate seam (the AgentSpawnOutcome type + process-host spawn bridge). Their reports:\n\n${JSON.stringify(done.map((r) => ({ fix: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 500) })), null, 2)}\n\nThe two halves were written concurrently against a shared SEAM contract (enum AgentSpawnOutcome { Completed(Value), Failed, Rejected(String) } + AgentSpawnFuture over it; cell_actor maps Rejected -> JS throw). Your job: RECONCILE the seam so both halves link, then FIX any breakage (compile errors, type mismatches at the seam, fmt, clippy, failing tests) WITHOUT changing intended behavior:\n1. cargo check -p codex-core -p codex-code-mode -p codex-code-mode-protocol -p codex-code-mode-host -p codex-app-server — fix any seam mismatch (e.g. the two agents named/shaped the outcome type differently; unify on the SEAM contract).\n2. cargo fmt; 'cargo fmt --check' passes.\n3. cargo clippy --all-targets for codex-core, codex-code-mode, codex-code-mode-protocol, codex-code-mode-host.\n4. cargo test -p codex-code-mode (incl. tests/agent_dispatch.rs) -p codex-code-mode-protocol; targeted codex-core spawn_await + scheduler + workflow + dispatch + nickname tests (RUST_MIN_STACK=8388608).\n5. Confirm the end-to-end behavior the fixes intend: a scheduler lifetime-cap breach yields a JS throw (Rejected), not null; the process-host bridge resolves agent() to a real value (not always null). If a test asserting this is missing or wrong, fix it.\nNEVER git commit/add/push. Return structured result, ticket "wave3-fix-verify"; tests_passed=true only if all green (modulo the documented pre-existing rollout_budget stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
