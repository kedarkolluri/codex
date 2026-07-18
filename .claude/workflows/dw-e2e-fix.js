export const meta = {
  name: 'dw-e2e-fix',
  description: 'Root-cause and fix the end-to-end failure where a workflow does not run through the real codex binary ("Expected exactly one workflow runner, found: []"), then prove it by running the binary',
  phases: [
    { title: 'Diagnose', detail: 'trace the real code-mode execution path; find why the model-callable workflow tool has no runner' },
    { title: 'Fix', detail: 'wire the workflow runner into the assembled path + add a real-binary smoke test' },
    { title: 'Prove', detail: 'rebuild the binary and actually run a workflow through codex exec' },
  ],
}

// Runs in an ISOLATED git worktree (branch claude/dw-e2e-fix off the M2 tip) so it never
// collides with the concurrently-running M3 workflow in the main tree.
const REPO = '/tmp/claude-1000/-home-kedar-projects-codex-codex/5ff61243-6408-49f3-a17e-5a5da2a4fcb9/scratchpad/e2e-wt'
const RS = REPO + '/codex-rs'

const DIAG = {
  type: 'object',
  additionalProperties: false,
  required: ['root_cause', 'evidence', 'fix_plan', 'files_to_change'],
  properties: {
    root_cause: { type: 'string' },
    evidence: { type: 'array', items: { type: 'string' } },
    fix_plan: { type: 'string' },
    files_to_change: { type: 'array', items: { type: 'string' } },
    notes: { type: 'string' },
  },
}

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

phase('Diagnose')
log('Root-causing the "Expected exactly one workflow runner, found: []" e2e failure')
const diag = await agent(
  `The Dynamic Workflows feature (branch claude/dynamic-workflows-impl, workspace ${RS}) passes all its hermetic fixture-lane UATs, but FAILS end-to-end through the REAL assembled codex binary. Reproduction: build \`${RS}/target/debug/codex\` (already built), then run:
  CODEX_HOME=<fresh tempdir with auth.json> ${RS}/target/debug/codex exec --enable workflow --skip-git-repo-check --sandbox workspace-write -C ${RS} "call the workflow tool once with source: export const meta = { name: 'smoke', description: 'd', phases: ['run'] }\\nphase('run')\\nlog('hi')\\n'done'"
The model correctly calls the \`workflow\` tool, but the runtime returns:
  Script error: Error: Expected exactly one workflow runner, found: []

CRITICAL CLUE: that exact error string is NOT in the Rust source (grep confirms). So it is generated in the JavaScript-side code-mode execution path — likely the JS wrapper/prelude/module-loader that runs a model-authored code-mode script expects exactly one "runner" (probably an exported/registered function the code-mode 'tools object' model expects), and the workflow host tool path (CodeModeWorkflowHandler, ${RS}/core/src/tools/code_mode/workflow_handler.rs) supplies a raw workflow script that has no such runner.

YOUR JOB — DIAGNOSE ONLY (do not fix): Trace precisely why. Investigate:
- How the PLAIN model-callable code-mode exec tool wraps/executes model source vs how CodeModeWorkflowHandler runs a workflow body. Find where "runner" comes from (search the code-mode crate, any embedded/generated JS, the module_loader, the 'build_tools_object'/description machinery, code-mode-protocol description.rs parse_exec_source, and how ExecuteRequest.source is transformed before evaluate_main_module). The plain code-mode path presumably injects or expects a runner; the workflow path bypasses/omits it.
- Whether the workflow tool should run the body directly (top-level await) WITHOUT the code-mode 'runner' wrapper, and where that wrapper is being applied to workflow source incorrectly.
- Why the fixture-lane tests miss it (they call run_workflow_source / the service directly rather than going through the assembled model-callable tool + code_mode_service the binary uses).
Use grep/read freely. Reproduce the failure yourself with the binary if helpful (export PATH="$HOME/.cargo/bin:$PATH"; the binary is at ${RS}/target/debug/codex; copy ~/.codex/auth.json into a fresh CODEX_HOME). Return the structured diagnosis: the exact root cause with file:line evidence, a concrete minimal fix plan, and the files to change. Do NOT edit any file.`,
  { label: 'diagnose', phase: 'Diagnose', schema: DIAG, model: 'opus' }
)
log('Root cause: ' + (diag ? diag.root_cause.slice(0, 160) : 'diagnosis failed'))

phase('Fix')
const fix = await agent(
  `Fix the Dynamic Workflows end-to-end failure in ${RS} (branch claude/dynamic-workflows-impl). A diagnosis agent root-caused why a workflow does not run through the real codex binary ("Expected exactly one workflow runner, found: []"):

ROOT CAUSE: ${diag ? diag.root_cause : '(diagnosis unavailable — re-investigate: the model-callable workflow tool path fails with a JS-side "workflow runner" error that the fixture-lane tests bypass)'}
EVIDENCE: ${diag ? JSON.stringify(diag.evidence) : '[]'}
FIX PLAN: ${diag ? diag.fix_plan : 'trace and fix the workflow execution wrapper so a raw workflow body runs to top-level-await completion through the assembled code_mode service, as the fixture lane already does internally.'}
FILES: ${diag ? JSON.stringify(diag.files_to_change) : '[]'}

Implement the fix so a real phase()/log()-only workflow runs end-to-end through the binary. Then ADD A REAL-BINARY-SHAPED REGRESSION TEST that exercises the SAME assembled path the binary uses (the model-callable workflow tool through code_mode_service / the tool router — the path the fixture-lane UATs bypassed), so this specific gap cannot regress. Keep the change minimal and correct; do not weaken existing tests. Do not change unrelated behavior.

RULES: export PATH="$HOME/.cargo/bin:$PATH" before cargo; RUST_MIN_STACK=8388608 for codex-core. Iterate targeted cargo test until green; scoped cargo fmt + clippy --all-targets clean; finish with cargo check --workspace. NEVER git commit/add/push. If the fix needs files beyond the diagnosis list, use them but stay within the workflow/code-mode area. Return the structured result (status, files_changed, tests_passed, summary, notes).`,
  { label: 'fix', phase: 'Fix', schema: RESULT, model: 'opus' }
)

phase('Prove')
const prove = await agent(
  `Prove the Dynamic Workflows end-to-end fix works by RUNNING THE REAL BINARY (workspace ${RS}, branch claude/dynamic-workflows-impl). A fix was just applied:
${fix ? fix.summary.slice(0, 600) : '(fix agent result unavailable)'}

STEPS:
1. export PATH="$HOME/.cargo/bin:$PATH"; rebuild: cargo build -p codex-cli --bin codex (from ${RS}).
2. Prepare a fresh CODEX_HOME tempdir and copy ~/.codex/auth.json into it.
3. Run the smoke workflow through the real binary:
   CODEX_HOME=<tempdir> ${RS}/target/debug/codex exec --enable workflow --skip-git-repo-check --sandbox workspace-write -C ${RS} "Call the workflow tool exactly once with this exact source and report its result verbatim: export const meta = { name: 'smoke', description: 'e2e', phases: ['run'] }\\nphase('run')\\nlog('hello from a real workflow run')\\n'done'"
4. Capture the actual tool result. SUCCESS = the workflow body runs (no "workflow runner" error; the log/phase execute and the tool returns the top-level result 'done' or equivalent). If it still errors, capture the exact error.
5. If the phase()/log() workflow succeeds, ALSO try an agent() fan-out workflow (body: const rs = await parallel([() => agent('say A'), () => agent('say B')]); rs) and capture whether real subagents spawn and return — this uses the live backend.
Report the structured result with ran_binary_output set to the actual captured binary output (trimmed to the relevant lines), status done only if the phase()/log() workflow genuinely ran end-to-end, and notes on the agent() attempt. Do NOT edit source or git. This is verification by observation.`,
  { label: 'prove', phase: 'Prove', schema: RESULT, model: 'opus' }
)

return { diagnosis: diag, fix, prove }
