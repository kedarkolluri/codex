export const meta = {
  name: 'dw-wave1-fixes',
  description: 'Apply codex-review fixes to the wave-1 Dynamic Workflows working-tree changes (parser hardening, schema wiring, budget visibility/reminders, journal invariants, bazel)',
  phases: [
    { title: 'Fix', detail: 'four parallel fix agents over disjoint review findings' },
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

const FIXES = [
  {
    id: 'fix-meta-parser-hardening',
    scope: 'codex-rs/code-mode-protocol/src/workflow_meta.rs (and lib.rs only if exports change)',
    findings: `Code-review findings to fix in the static workflow-meta parser (security-critical, parses untrusted files during discovery):
1. BLOCKER — recursive object/array parsing has no depth limit; deeply nested arrays (tens of thousands) under an ignored key can stack-overflow discovery. Enforce a hard nesting-depth limit (e.g. 32) and a hard cap on the scanned manifest region size, with clear errors.
2. MAJOR — the parser stops after the first object literal without validating the statement terminator: 'export const meta = {…} && buildMeta();' is accepted. After the closing brace, allow only whitespace/comments then an optional ';' then end-of-statement (newline/EOF/next statement). Any trailing operator or expression must reject.
3. MAJOR — bare 'undefined' is treated as literal null; 'extra: undefined' passes. Reject 'undefined' as a non-literal reference.
4. MAJOR — Parser::new materializes the ENTIRE script (including a possibly huge body) into Vec<char>. Rework the cursor so it does not allocate proportional to the whole file (iterate char_indices over &str, or bound the scanned window) — parsing must not read past the end of the meta statement, and memory must stay O(manifest region).
5. MINOR — \\uD83D\\uDE00-style surrogate-pair escapes are rejected one code unit at a time; combine valid surrogate pairs into the astral char, still rejecting lone surrogates.
6. MINOR (mitigate) — the grammar is broader than {name, description, phases}; KEEP the general-literal grammar (forward-compat with phase objects/whenToUse) but add adversarial tests: deep nesting at/over the new limit, huge-body-after-manifest (assert bounded work), the '&& call()' bypass, 'undefined' rejection, lone-surrogate rejection, and surrogate-pair acceptance.`,
  },
  {
    id: 'fix-features-config-schema',
    scope: 'codex-rs/config/src/schema.rs, the checked-in config.schema.json fixture that core/src/config/schema_tests.rs compares against, and (only if needed) codex-rs/features',
    findings: `Code-review finding: WorkflowConfigToml was added to the features crate (already in the working tree) but NOT wired into the typed config schema — config/src/schema.rs:33 references codex_features::CodeModeConfigToml and siblings; workflow is missing, so the committed config.schema.json rejects '[features.workflow] enabled = true'. Add the workflow entry everywhere its code_mode/multi_agent_v2 siblings appear in the schema plumbing, then regenerate the checked-in config.schema.json fixture the same way the repo does (read core/src/config/schema_tests.rs to find the bless/regen mechanism — there is likely an env var or a just recipe; codex-rs/justfile may have it). Prove it with the schema tests going green: a TOML doc with [features.workflow] enabled=true must validate against the regenerated schema.`,
  },
  {
    id: 'fix-budget-visibility-reminders',
    scope: 'codex-rs/core/src/rollout_budget.rs only',
    findings: `Code-review findings on the working-tree budget changes:
1. MINOR — spent()/remaining() are pub(crate) but the plan acceptance says 'pub fn spent()' / 'pub fn remaining()' (they become the native backing for the JS budget global, called from outside codex-core later). Make them pub and drop any now-unneeded allow(dead_code).
2. MINOR — on reconfigure, per-thread reminder delivery indices are preserved even when limit/thresholds change, so an old delivery can suppress the first reminder of the NEW configuration. When configure() changes the config (limit or weights or reminder thresholds), reset the reminder-delivery bookkeeping while STILL preserving weighted_tokens_used (accumulated spend must survive — that is the whole point of the resettable cell). A reconfigure with an identical config should change nothing. Add tests for both behaviors.`,
  },
  {
    id: 'fix-journal-invariants-bazel',
    scope: 'codex-rs/workflow-journal/ (lib.rs, Cargo.toml, new BUILD.bazel), and MODULE.bazel.lock at repo root only if the update tool works',
    findings: `Code-review findings on the new workflow-journal crate (types for journal.jsonl per spec §7):
1. MAJOR — PhaseLine/LogLine model the §7 invariant 'ordinal: null' as Option<u64>, so callers can serialize ordinal:7 which §7 forbids. Encode the null-only invariant in the type (e.g. a zero-sized NullOrdinal marker that always serializes as null and only deserializes from null), keeping the wire bytes identical to the §7 samples.
2. MAJOR — required replay/linkage fields are nullable: a status:"completed" agent_call line can carry tokens_spent:null (breaking budget re-add on resume) or missing child_thread_id/rollout_path (breaking journal-only transcript grouping). Keep the serde shapes §7-sample-compatible, but add a pub fn validate(&self) -> Result<(), String> (on JournalLine and/or AgentCallLine) enforcing status-dependent invariants: completed => tokens_spent, child_thread_id and rollout_path present; document that the recorder (later ticket) must call it. Unit-test accept/reject cases.
3. MAJOR — the crate has no BUILD.bazel. Add one copying the boilerplate pattern from codex-rs/features/BUILD.bazel (codex_rust_crate with the standard compile_data glob, crate_name codex_workflow_journal). Then try the repo's lock update ('just bazel-lock-update' from codex-rs, or scripts/check-module-bazel-lock.sh to check); if bazel is not installed on this machine, note that in your result rather than failing — do NOT hand-edit MODULE.bazel.lock.
NOT in scope (deliberately dismissed): making timestamps required — Option timestamps are a design decision so §7 sample lines round-trip byte-exact; leave as-is.`,
  },
]

const buildPrompt = (f) => `You are applying code-review fixes to UNCOMMITTED working-tree changes in the codex repo at ${REPO} (branch claude/dynamic-workflows-impl; Rust workspace ${REPO}/codex-rs). The working tree contains the wave-1 Dynamic Workflows implementation (docs/dynamic-workflows-plan.md is the plan; docs/dynamic-workflows-spec.md the spec, esp. §7 for the journal format).

Your fix bundle: ${f.id}

${f.findings}

RULES:
- Toolchain: run 'export PATH="$HOME/.cargo/bin:$PATH"' before every cargo command.
- Only modify files within this scope: ${f.scope}. Other agents are concurrently fixing OTHER files in this same working tree — never touch their areas, never revert existing working-tree changes, and NEVER run git commit/add/push/stash/checkout.
- First READ the current implementation and its tests to understand what is there; the review findings describe real defects — verify each against the code as you fix it. If you conclude a finding is wrong, say so in notes with evidence instead of forcing a change.
- Update/extend the existing unit tests; iterate targeted 'cargo test -p <package>' until green. Then scoped cargo fmt + 'cargo clippy -p <package> --all-targets' clean.
- Cargo may block on the shared target-dir lock — wait it out. RUST_MIN_STACK=8388608 works around the PRE-EXISTING codex-core tests/all.rs rollout_budget stack overflow (not yours to fix).
- If genuinely blocked, return status "blocked" with an explanation in notes.

Return the structured result; files_changed = every file you created or modified (repo-relative).`

phase('Fix')
log('Fanning out 4 fix agents over the codex-review findings')
const results = await parallel(
  FIXES.map((f) => () => agent(buildPrompt(f), { label: f.id, phase: 'Fix', schema: RESULT, model: 'opus' }))
)

const done = results.filter(Boolean)
log('Fixes done: ' + done.filter((r) => r.status === 'done').length + '/' + FIXES.length)

phase('Verify')
const verify = await agent(
  `In the codex repo at ${REPO} (workspace ${REPO}/codex-rs; 'export PATH="$HOME/.cargo/bin:$PATH"' before cargo), review fixes were just applied to the uncommitted wave-1 Dynamic Workflows changes:\n\n${JSON.stringify(done.map((r) => ({ fix: r.ticket, status: r.status, files: r.files_changed, summary: r.summary.slice(0, 400) })), null, 2)}\n\nRun and FIX any breakage (without changing intended behavior):\n1. cargo fmt; 'cargo fmt --check' must pass\n2. cargo clippy --all-targets for codex-features, codex-code-mode-protocol, codex-workflow-journal\n3. cargo test -p codex-features -p codex-code-mode-protocol -p codex-workflow-journal\n4. cargo test -p codex-core --lib rollout_budget\n5. The config schema tests that cover config.schema.json (find them via core/src/config/schema_tests.rs; run that test target)\n6. cargo check -p codex-core\nNEVER git commit/add/push. Return structured result, ticket "wave1-fix-verify"; tests_passed=true only if all green.`,
  { label: 'verify-sweep', phase: 'Verify', schema: RESULT, model: 'opus' }
)

return { results: done, verify }
