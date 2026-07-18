export const meta = {
  name: 'dw-wave2-fixes',
  description: 'Fix the verified wave-2 codex-review findings (spawn_await event race, loader resource caps, watcher parity/gating, handler polish)',
  phases: [
    { title: 'Fix', detail: 'four parallel fix agents over disjoint findings' },
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
    id: 'fix-spawn-await-event-race',
    scope: 'codex-rs/core/src/agent/control/spawn_await.rs, spawn_await_tests.rs, core/src/session/mod.rs (event tap only), and minimal thread plumbing the tap needs',
    findings: `BLOCKER (verified with evidence): spawn_await.rs:90 drains child_thread.next_event(), which bottoms out at session/mod.rs:819-826 self.rx_event.recv() where rx_event is an async_channel (MPMC work-stealing: each event goes to exactly ONE competing recv() caller; constructed at session/mod.rs:539). The app-server auto-attaches a competing consumer to every registered thread: notify_thread_created (spawn.rs:383) -> app-server/src/lib.rs:1084 thread_created_rx.recv() -> try_attach_thread_listener (thread_processor.rs:2633, no subagent filter) -> ensure_conversation_listener -> listener task looping conversation.next_event() (thread_lifecycle.rs:302). So in production the app-server listener can steal EventMsg::TurnComplete and the helper waits forever. tasks/review.rs escapes only because the one-shot delegate owns a private receiver (review.rs:127-149) — not available on the registering path.
FIX: give the helper a NON-COMPETING event tap instead of stealing recv()s. Recommended: add a broadcast-based observer to Session — at the single point where session events are posted into tx_event, also publish into a tokio::sync::broadcast::Sender<Event> (lag-tolerant, capacity generous e.g. 1024); expose subscribe via CodexThread (e.g. pub fn subscribe_events(&self) -> broadcast::Receiver<Event>). spawn_await subscribes BEFORE submitting the child's input op, then awaits TurnComplete/TurnAborted on the tap, returning Some(last_agent_message)/None. next_event()/rx_event semantics for existing consumers must remain byte-for-byte unchanged (app-server keeps sole ownership of the MPMC receiver). Handle broadcast Lagged by treating it as continue (scan for terminal events; on lag+stream-close without terminal event return None). If while implementing you find an existing equivalent tap/observer mechanism, use it instead of inventing one — search first.
MAJOR (verified): spawn_await.rs:53 forwards session_source: Option<SessionSource>; None reaches state.spawn_new_thread (spawn.rs:328) NOT spawn_new_thread_with_source(ThreadSource::Subagent), and persist_thread_spawn_edge_for_source early-returns without a parent (control.rs:689-692) so NO spawn edge is written. FIX: make the Subagent source intrinsic — the helper takes the parent thread id (non-optional) and constructs SessionSource::SubAgent(SubAgentSource::ThreadSpawn{..}) itself so every caller gets the registering + edge-writing path by construction.
MINOR (verified): tests cover pre-spawn error but not TurnAborted, and no test attaches a competing drain. FIX: add (a) a TurnAborted -> None test, (b) a race regression test that spawns a competing task continuously draining child_thread.next_event() (simulating the app-server listener) and asserts the helper STILL returns the final message.`,
  },
  {
    id: 'fix-loader-resource-caps',
    scope: 'codex-rs/core-workflows/ (loader.rs, model.rs, tests)',
    findings: `MAJOR (verified): discovery walks untrusted repos with no candidate-count or total-work cap (loader.rs:113) and reads every candidate .js fully into memory (loader.rs:125) — one multi-GB file or thousands of files can stall/OOM app-server re-discovery. FIX: (1) read at most a bounded prefix of each file (the static meta parser already enforces a 256KiB manifest scan cap — parse_workflow_meta only needs the manifest region; read via File::take(MAX_MANIFEST_BYTES as u64 + slack) so a huge body is never materialized; a file whose meta region exceeds the cap fails open and is skipped-with-recorded-error); (2) cap candidates per discovery pass (e.g. MAX_WORKFLOW_FILES_PER_ROOT = 256, deterministic order, log/record what was dropped — no silent truncation); (3) keep fail-open semantics. Tests: huge-file-is-bounded (create a file with small valid meta + >10MB body, assert discovery succeeds fast and the entry parses; and a >cap meta region is skipped), too-many-files cap with recorded drop, existing 7 tests stay green.`,
  },
  {
    id: 'fix-watcher-parity-gating',
    scope: 'codex-rs/app-server/src/workflows_watcher.rs, message_processor.rs, and (if a shared service is added) a small workflows service module in app-server; core-workflows only for API additions the service needs',
    findings: `Three verified findings on the workflows watcher vs the skills precedent:
1. MAJOR: watcher starts unconditionally (message_processor.rs:310-312) even with Feature::Workflow disabled — an off experimental feature still watches repos and parses untrusted files. FIX: gate construction/start on config.features.enabled(Feature::Workflow) (Config.features: ManagedFeatures is in scope at that call site; features/src/lib.rs:152 has the variant). When disabled: no watcher, no notification, zero filesystem activity. Test both states.
2. MAJOR: roots are fixed at construction from startup config (workflows_watcher.rs:40-49,78: startup cwd/.codex/workflows + $HOME/.agents/workflows + $CODEX_HOME/workflows), while the skills precedent derives roots per-thread at listener-attach time via register_thread_config (skills_watcher.rs:80-125) from config.cwd + config_layer_stack. FIX: mirror the skills mechanism — register workflow roots per thread config when a thread attaches (a register_thread_config analog invoked from the same place skills' is: ensure_listener_task_running, thread_lifecycle.rs:230-236), so a thread opened in repo/subdir contributes <that repo>/.codex/workflows. Keep the static $HOME/$CODEX_HOME roots from construction. Follow the skills watcher's dedupe/unregister lifecycle.
3. MAJOR: on change the watcher re-runs discovery, drops the registry, and only logs (workflows_watcher.rs:118-127) — no shared cache is invalidated, unlike skills which calls skills_service.clear_cache() then emits (skills_watcher.rs:148-153). FIX: either (preferred, precedent-faithful) introduce a minimal shared WorkflowsService holding a cached WorkflowRegistry with clear_cache()/load-on-demand, invalidated by the watcher before emitting WorkflowsChanged — the P4 entrypoint tickets will consume it; or, if a service is genuinely premature, DELETE the pointless inline re-discovery and emit only, with a comment saying clients re-query (pick one and justify in notes; do not keep discover-and-drop).
Update/extend the workflows_watcher integration test for the gating + per-thread-root behavior.`,
  },
  {
    id: 'fix-handler-polish',
    scope: 'codex-rs/core/src/tools/code_mode/ (workflow_spec.rs, workflow_handler.rs, mod.rs tests)',
    findings: `Two verified minor findings on the workflow host tool skeleton:
1. workflow_spec.rs:29: the model-visible tool description promises a "deterministic isolate" but determinism hardening (Date/Math/timers) is explicitly deferred to M3 tickets. FIX: reword the description to what the runtime delivers TODAY (runs a meta-validated workflow script once in a fresh isolate; orchestration hooks arriving in later milestones); no determinism claim.
2. mod.rs:469: the claimed end-to-end tests call the internal run_workflow_source helper directly, never exercising CodeModeWorkflowHandler as a registered tool (payload matching, tool-plan registration, model-facing response adaptation). A registration/gating regression would pass current tests. FIX: add a handler-level test that goes through the ToolExecutor/registration surface used by build_code_mode_executors (mirror how existing code-mode executor tests drive execute_handler end-to-end, if such precedent exists — search for it): assert (a) with Feature::Workflow enabled the "workflow" tool dispatches a trivial meta-valid script and returns its result through the model-facing adapter, (b) with the feature disabled the tool is absent/unreachable, (c) an invalid-meta script is rejected before isolate execution at the handler surface.`,
  },
]

const buildPrompt = (f) => `You are applying verified code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; workspace ${REPO}/codex-rs). The tree holds the wave-2 Dynamic Workflows implementation (plan: docs/dynamic-workflows-plan.md; spec: docs/dynamic-workflows-spec.md). Every finding below was independently verified against the code with file:line evidence — treat them as real, but re-verify locally as you fix; if you conclude one is wrong, push back in notes with evidence instead of forcing a change.

Your fix bundle: ${f.id}

${f.findings}

RULES:
- 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only modify files within this scope: ${f.scope}. Other agents are concurrently fixing OTHER files — never touch their areas, never revert working-tree changes you did not write, NEVER run git commit/add/push/stash/checkout.
- Read the current implementation and the named precedents (file:line) before changing anything.
- Update/extend tests; iterate targeted 'cargo test -p <package>' until green; scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean. RUST_MIN_STACK=8388608 for codex-core integration binaries (pre-existing rollout_budget stack overflow is not yours). App-server integration test target is 'all'.
- Cargo may block on the shared target-dir lock — wait it out. If genuinely blocked, return status "blocked" with notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 4 fix agents over the verified wave-2 review findings')
const results = await parallel(
  FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' }))
)

const done = results.filter(Boolean)
log('Fixes done: ' + done.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), review fixes were just applied to the uncommitted wave-2 changes:\n\n${JSON.stringify(done.map((r) => ({ fix: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRun and FIX any breakage (without changing intended behavior):\n1. cargo fmt; 'cargo fmt --check' passes\n2. cargo clippy --all-targets for codex-core-workflows, codex-app-server, codex-core, codex-code-mode\n3. cargo test -p codex-core-workflows; cargo test -p codex-code-mode; targeted spawn_await + workflow-handler + workflows_watcher tests (RUST_MIN_STACK=8388608; app-server test target 'all')\n4. cargo check -p codex-core -p codex-app-server\nNEVER git commit/add/push. Return structured result, ticket "wave2-fix-verify"; tests_passed=true only if all green (modulo the documented pre-existing rollout_budget stack overflow).`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
