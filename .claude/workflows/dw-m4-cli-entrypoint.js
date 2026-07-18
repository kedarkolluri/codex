export const meta = {
  name: 'dw-m4-cli-entrypoint',
  description: 'Build the `codex workflow run` CLI entrypoint (P4-cli-run) so a workflow can actually be invoked, then PROVE the M0-M3 engine end-to-end by running the real binary',
  phases: [
    { title: 'Implement', detail: 'codex workflow run <file> [--args json] [--resume runId]' },
    { title: 'Prove', detail: 'rebuild codex + host, run a real workflow through the CLI' },
  ],
}

const REPO = '/home/kedar/projects/codex/codex'
const RS = REPO + '/codex-rs'

const RESULT = {
  type: 'object',
  additionalProperties: false,
  required: ['status', 'files_changed', 'tests_passed', 'summary'],
  properties: {
    status: { type: 'string', enum: ['done', 'blocked'] },
    files_changed: { type: 'array', items: { type: 'string' } },
    tests_passed: { type: 'boolean' },
    summary: { type: 'string' },
    ran_binary_output: { type: 'string' },
    notes: { type: 'string' },
  },
}

phase('Implement')
log('Building the `codex workflow run` CLI entrypoint (P4-cli-run)')
const impl = await agent(
  `Implement ticket P4-cli-run (milestone M4) of Dynamic Workflows in ${RS} (branch claude/dynamic-workflows-impl; M0-M3 committed). Toolchain: export PATH="$HOME/.cargo/bin:$PATH" before cargo; RUST_MIN_STACK=8388608 for codex-core.

WHY THIS IS URGENT (root cause of the end-to-end failure, GitHub #28): the Dynamic Workflows ENGINE (parse meta, run body once in a fresh isolate, agent()/parallel()/pipeline()/phase()/log()/args/budget/workflow(), scheduler, journal, resume) is fully built and unit/integration-tested via core/src/tools/code_mode/workflow_handler.rs::run_workflow_source, BUT there is NO way to invoke it: no model-callable workflow tool and no CLI command exist (both are M4). Proof: running a workflow via the model just runs the source through the plain code-mode \`exec\` tool with workflow:false, so \`phase\` is not defined. Your job is to add the CLI entrypoint so the engine is actually reachable.

READ FIRST: (1) ${REPO}/docs/dynamic-workflows-plan.md section "#### \\\`P4-cli-run\\\`" — its description + "_Acceptance:_"; (2) spec §9 (entrypoints); (3) how \`codex exec\` bootstraps a Config + Session/thread and runs (the \`exec\` crate main + lib, and how core builds a thread/session with a code_mode_service). Mirror that bootstrap.

IMPLEMENT: a \`codex workflow run <name|path> [--args <json>] [--resume <runId>]\` subcommand (Subcommand::Workflow scaffold in the cli/arg0/exec layer):
- Resolve <path> to a workflow script file (also accept a saved name via codex_core_workflows::resolve_by_name — the loader from #5 — but a raw .js path is the primary case).
- Bootstrap a Config + Session the same way \`codex exec\` does (respect --enable workflow / feature flags; the feature transitively enables CodeMode + MultiAgentV2). CRITICAL: the workflow body must run with workflow:true so the workflow globals install — call the CodeModeWorkflowHandler / run_workflow_source path directly (NOT the plain exec tool). Do NOT require the model: a phase()/log()-only workflow must run with ZERO model calls.
- Thread --args (JSON) into the run as \`args\`; wire --resume <runId> to the P3 resumeFromRunId entrypoint (already built in workflow_handler.rs) if present.
- Surface phase()/log() output and the workflow's top-level return value to stdout; non-zero exit on workflow error.
- Keep it minimal but REAL and correct; reuse existing session/exec plumbing rather than reinventing. If run_workflow_source needs an ExecContext/Session you must construct, mirror exactly how the code_mode exec path builds it.

TEST: add a test that invokes the new subcommand end-to-end on a phase()/log() workflow (through the assembled CLI path, e.g. an exec-crate integration test like the existing codex exec tests) asserting it runs with workflow globals installed and no model — this is the real-entrypoint coverage that was missing. Iterate targeted cargo test until green; scoped cargo fmt + clippy --all-targets clean; finish with cargo check --workspace. NEVER git commit/add/push. If the full entrypoint is too large, implement the smallest correct version that makes \`codex workflow run <path>\` run a phase()/log() workflow end-to-end, and note what you deferred (e.g. saved-name resolution, --resume). Return the structured result.`,
  { label: 'cli-run', phase: 'Implement', schema: RESULT, model: 'opus' }
)

phase('Prove')
const prove = await agent(
  `Prove the new \`codex workflow run\` CLI entrypoint works by RUNNING THE REAL BINARY (workspace ${RS}, branch claude/dynamic-workflows-impl). It was just implemented:
${impl ? impl.summary.slice(0, 700) : '(impl result unavailable)'}

STEPS (export PATH="$HOME/.cargo/bin:$PATH" first):
1. Rebuild BOTH binaries from the current tree (a stale host binary caused an earlier misdiagnosis): cargo build -p codex-cli --bin codex AND cargo build -p codex-code-mode-host. Confirm both land in ${RS}/target/debug/.
2. Write a smoke workflow file /tmp/wf-smoke.js:
     export const meta = { name: 'smoke', description: 'e2e', phases: ['run'] }
     phase('run')
     log('hello from a real workflow run')
     'done'
3. Run it through the NEW CLI: ${RS}/target/debug/codex workflow run /tmp/wf-smoke.js --enable workflow  (adjust flags to the actual subcommand syntax the impl added; check \`codex workflow run --help\`). Use a fresh CODEX_HOME tempdir. This needs NO model.
4. Capture the actual output. SUCCESS = the workflow runs (phase/log execute, top-level 'done' returned, NO "phase is not defined" / no "workflow runner" error). If it errors, capture the exact error verbatim.
5. If the phase()/log() run succeeds, ALSO try an agent() fan-out workflow file (body: const rs = await parallel([() => agent('reply with the letter A'), () => agent('reply with the letter B')]); rs) — copy ~/.codex/auth.json into the CODEX_HOME so it can reach the live backend — and capture whether real subagents spawn and return.
6. If --resume was wired, run the same script twice and note whether the second run replays from the journal.
Set ran_binary_output to the ACTUAL captured CLI output (trimmed to the relevant lines), status done ONLY if the phase()/log() workflow genuinely ran end-to-end through the CLI, and notes on the agent()/resume attempts. Do NOT edit source or git — verification by observation only.`,
  { label: 'prove', phase: 'Prove', schema: RESULT, model: 'opus' }
)

return { impl, prove }
