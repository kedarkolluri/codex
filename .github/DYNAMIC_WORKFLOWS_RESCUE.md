# Dynamic Workflows rescue charter

This document is the durable handoff and execution charter for completing Dynamic
Workflows in the `kedarkolluri/codex` fork. It records the product objective, the
recovered state from the interrupted implementation, the remaining work, the UAT
and Claude-parity requirements, and the delivery constraints that keep future
OpenAI upstream rebases manageable.

The implementation design remains:

- [Dynamic Workflows specification](../docs/dynamic-workflows-spec.md)
- [Dynamic Workflows execution plan](../docs/dynamic-workflows-plan.md)

The tracker epic is [kedarkolluri/codex#1](https://github.com/kedarkolluri/codex/issues/1).

## Mission

Deliver Dynamic Workflows for Codex with empirically measured Claude Code parity:

- host-authored JavaScript workflows in fresh V8 isolates;
- deterministic `agent`, `parallel`, `pipeline`, `phase`, `log`, `workflow`,
  `args`, and `budget` behavior;
- scheduling, limits, structured outputs, nested workflows, journaling, and
  prefix-replay resume;
- saved-workflow discovery and reload;
- three entrypoints: `codex workflow run`, model-callable `workflow_run`, and
  interactive `/workflow`;
- background execution with a live TUI monitor, child-agent drill-in and return,
  persistent child sessions, completion notifications, and deterministic
  worktree isolation;
- deterministic automated tests plus agents operating the real TUI as human
  users; and
- a black-box parity comparison against the installed Claude Code version, with
  captured evidence rather than assumed behavior.

Fork completion and upstream contribution are separate deliverables. The fork
must become a dependable daily workhorse while the upstream proposal is split
into small, maintainer-aligned changes that remain easy to rebase.

## Delivery tracker

The GitHub hierarchy is rooted at
[epic #1](https://github.com/kedarkolluri/codex/issues/1). The issue bodies carry
the detailed acceptance criteria and GitHub sub-issue relationships carry
progress into the parent epics.

The public human-facing board is
[Codex Dynamic Workflows Project #3](https://github.com/users/kedarkolluri/projects/3).
It tracks Phase, Workstream, Lane, Effort, Priority, assignees, parent issues,
and sub-issue progress for the full delivery tree.

- [#29](https://github.com/kedarkolluri/codex/issues/29) preserves the interrupted
  session and establishes the baseline.
- [#24](https://github.com/kedarkolluri/codex/issues/24) owns M4 observability,
  entrypoints, session navigation, persistence, and worktree isolation. Its
  implementation children are
  [#28](https://github.com/kedarkolluri/codex/issues/28) and
  [#30–#48](https://github.com/kedarkolluri/codex/issues/30).
- [#25](https://github.com/kedarkolluri/codex/issues/25) owns deterministic tests,
  human-agent UAT, cross-host conformance, CI, and Claude comparison. Its
  children are [#49–#55](https://github.com/kedarkolluri/codex/issues/49) and
  [#57](https://github.com/kedarkolluri/codex/issues/57).
- [#56](https://github.com/kedarkolluri/codex/issues/56) owns the rebase-friendly
  architecture audit and staged upstream strategy.

Immediate critical path:

1. Preserve the forensic objects in #29.
2. Repair and prove the real CLI/production-host seam in #28 and #57.
3. Build the protocol and run-model foundation in #32, #30, #33, and #31.
4. Complete the M4 user experience under #24.
5. Complete deterministic plus human-agent UAT and Claude evidence under #25.
6. Rehearse upstream synchronization and produce the staged proposal in #56.

## Recovered repository state

Snapshot taken on 2026-07-18:

- repository: `/home/kedar/projects/codex/codex`;
- branch: `claude/dynamic-workflows-impl`;
- HEAD: `873a52111593873f3f6e406d560fb64542f13efe`;
- HEAD matches `origin/claude/dynamic-workflows-impl`;
- only the `kedarkolluri/codex` fork remote is configured;
- the branch is 39 commits ahead of fork `main` and changes roughly 126 files,
  with about 25,404 insertions and 64 deletions; and
- at recon time, it was roughly 46 commits ahead and 193 commits behind OpenAI
  `main`.

This is much too large and divergent for one upstream pull request.

### Milestone state

- M0 foundations are committed: feature flag, transitive dependencies, static
  metadata parser, workflow loader/watcher, code-mode host skeleton, and
  `phase()`/`log()`.
- M1 MVP orchestration is committed: `agent()`, `parallel()`, scheduler, options,
  structured schema, deterministic nicknames, limits, args, and per-agent
  sessions.
- M2 scheduling and governance is committed: no-barrier `pipeline()`, hard budget
  behavior, and one-level `workflow()` nesting.
- M3 determinism and resume is implemented: deterministic-runtime hardening,
  journal storage/indexing, and prefix-replay resume. Fixture coverage exists,
  but production-host and real-binary validation is incomplete.
- M4 observability, entrypoints, and isolation is mostly incomplete.
- MT testing and CI is incomplete beyond partial fixture and ad hoc coverage.

### Dirty work inherited from the interrupted session

Modified:

- `.claude/workflows/dw-m4-cli-entrypoint.js`
- `codex-rs/cli/src/main.rs`
- `codex-rs/core/src/lib.rs`
- `codex-rs/core/src/tools/code_mode/mod.rs`

Untracked:

- `codex-rs/cli/tests/workflow_run.rs`
- `codex-rs/core/src/tools/code_mode/cli_entry.rs`

The dirty slice attempts:

```text
codex workflow run <name|path> [--args JSON] [--resume RUN_ID]
```

Do not commit this slice as-is. Its narrow `phase()`/`log()` smoke path works, but
the production architecture and security checks are incomplete.

Known problems:

1. The normal path chooses `ProcessOwnedCodeModeSessionProvider`, but the remote
   delegate does not transport the workflow journal/replay callbacks. Phase/log
   narration and real replay therefore disappear across the default production
   seam.
2. The standalone service does not establish the complete Session/Turn/tool
   worker path, so `agent()` and `parallel()` fanout are not proven and are
   expected to fail or stall.
3. Existing workflow UAT disables the process-owned code-mode host, hiding this
   mismatch.
4. Direct and saved workflow source reads bypass the existing size cap.
5. Arbitrary resume IDs can become path traversal inputs unless validated.
6. Root `-C`, configured cwd, and runtime `--profile` integration are incomplete.
7. Coverage is missing for saved names, exact narration and journals, both host
   modes, actual prefix replay, script failures, source limits, hostile run IDs,
   cwd/profile behavior, and real fanout.
8. Deep async command dispatch should return structured errors rather than call
   `std::process::exit`.

The `.claude` change only removes two hard-coded `model: 'opus'` selections from
an orchestration script. Keep it separate from product changes until it is
deliberately accepted.

Recovered logs report four CLI integration tests, 107 code-mode tests, scoped
Clippy, a workspace check, formatting, and one successful phase/log-only binary
invocation. Most used direct `cargo` commands, so they are evidence from the
interrupted session, not final repository-sanctioned gates.

## Forensic state to preserve

The former review worktree under `/tmp/claude-1000` is gone and its registration
is prunable. Its detached review snapshots remain readable at the time of this
handoff:

- `c6811df20`
- `ea36076e5`
- `3d21952bb`
- `1a07b6389`
- `226236fb2`
- `a611467ea`
- `4e5ac1b88`
- `4e91009bc`
- `0791ceaa1`

Two dropped stash commits also remain readable while `git stash list` is empty:

- `ad12fe5b4`
- `786490891`

Before worktree pruning, garbage collection, rebasing, or history surgery, pin
these objects under clearly named rescue refs or write them to a Git bundle. Do
not cherry-pick the detached M2 snapshot wholesale; the active branch contains
later refinements.

## Required product behavior

### Authoring and runtime

- Host-authored JavaScript ES modules.
- Static metadata export:

  ```javascript
  export const meta = { name, description, phases }
  ```

- Execute the workflow body exactly once in a fresh V8 isolate, not as an
  ordinary chat turn.
- Provide deterministic globals: `agent()`, `parallel()`, `pipeline()`,
  `phase()`, `log()`, `workflow()`, `args`, and `budget`.
- Disable or reject nondeterministic APIs including `Date.now`, argument-less
  `Date`, `Math.random`, `WeakRef`, `FinalizationRegistry`, and timer APIs.

### Agent execution

- Support `label`, `phase`, `schema`, `model`, `effort`,
  `isolation: 'worktree'`, and `agentType` options.
- Return `null` for death or skip according to the specification.
- `parallel()` is a barrier.
- `pipeline()` launches incrementally without an implicit barrier.
- Enforce the documented concurrency cap `min(16, cores - 2)`, queuing, a
  lifetime limit near 1000, and an item limit of 4096.
- Preserve child sessions and rollouts grouped under the workflow run.

### Governance and recovery

- Enforce the hard budget ceiling with exact ordinal behavior and no later
  spawn.
- Store append-only journals keyed by `runId`.
- Resume through the longest unchanged `(prompt, opts)` prefix.
- Never issue duplicate model or fixture calls for replayed operations.
- Restore budget accounting identically on replay.
- Permit one nested workflow level and reject a second.
- Discover saved workflows and hot reload changes.

### Product entrypoints

- `codex workflow run`, plus `ls` and `watch` where defined by the plan.
- Model-callable `workflow_run`.
- Interactive `/workflow` picker and launcher.
- Normalize all entrypoints to equivalent outcomes and journals.
- Feature-gate every surface consistently.

### App-server and TUI

- Add the necessary `Workflow*` protocol events.
- Add v2 app-server `workflow/*` notifications and mappings.
- Model workflow run, phase, and agent state explicitly.
- Render a `WorkflowProgressCell` with pending skeletons and
  pending-to-active-to-done transitions.
- Redraw progress in place instead of appending noisy duplicates.
- Let a user drill into a running child, observe streamed activity, and return to
  the live monitor without losing the parent subscription.
- Keep child sessions inspectable after completion.
- Emit a completion notification.
- Implement deterministic worktree allocation, cwd override, guards, scheduler
  integration, and cleanup.

## Execution sequence

### A. Preserve and baseline

1. Reconfirm branch, worktree, object reachability, and dirty diff.
2. Preserve the orphaned commits.
3. Separate product work from orchestration-only files.
4. Do not rebase, merge upstream, or clean the worktree yet.

### B. Repair the real execution seam

Trace the full lifecycle from CLI parsing through configuration, target
resolution, Session/Turn creation, host selection, delegate protocol, tool
routing, journal, replay, agent fanout, and output. Reuse existing execution
abstractions rather than maintaining a partial parallel runtime.

Phase/log narration, journal writes, replay entries, agent execution, errors, and
final output must behave equivalently through both in-process and process-owned
hosts. Build the main binary and code-mode host together so stale-sidecar failures
cannot produce false results.

### C. Complete M4 as reviewable slices

1. Protocol event model.
2. App-server v2 notifications and mapping.
3. Run/phase/agent state projection.
4. TUI monitor and snapshots.
5. Child-agent focus stack and return navigation.
6. CLI `run`, `ls`, and `watch`.
7. Model-callable `workflow_run`.
8. Interactive `/workflow` picker.
9. Worktree execution and cleanup.
10. Completion notification and persistence.

Keep normal changes below the repository's 800-line review guidance and complex
logic below roughly 500 changed lines where possible. Prefer new focused modules
over expanding central TUI and core orchestration files. Minimize new code in
`codex-core`; use narrower existing crates or introduce a focused crate when that
reduces long-term coupling.

## UAT architecture

Testing uses three separate planes.

### Deterministic fixture SUT

- Use deterministic Responses/SSE fixtures without uncontrolled model behavior.
- Exercise the real Session/Turn/tool-worker path.
- Run every relevant operation through both in-process and process-owned
  code-mode hosts.
- Produce structured, reproducible artifacts.

### Human-style drivers

- Build a deterministic in-process TUI driver using the real App, embedded
  app-server, and fixed VT100 backend for gating assertions.
- Separately assign an agent to act as a human user of the actual built TUI in a
  PTY or tmux session at a fixed terminal size.
- The human-driver agent may use only visible terminal output and keyboard
  input: launch Codex, type commands, use the workflow picker, watch progress,
  drill into a child, observe streaming output, navigate back, and wait for
  completion.
- It may not inspect Rust state or internal test APIs to decide whether the UI
  passed.
- Capture terminal frames or transcripts, keystrokes, dimensions, timing
  checkpoints, exit status, and observations.
- Provide CLI/NDJSON twins for headless scenarios.

### Judges

- Deterministic Rust assertions on normalized events, journals, and frames are
  required CI gates.
- A separate judge agent reviews captured frames/transcripts for usability and
  parity. This can be a non-gating or nightly lane, but the report is required
  before parity signoff.
- Driver and judge must be separate roles.

Each scenario needs deterministic fixtures, exact user actions, Rust assertions,
a headless twin where applicable, PTY evidence, a driver report, and an
independent judge result.

Required scenarios:

1. Live monitor pending to active to done, with in-place redraw.
2. Drill into a running child and return while the parent stays subscribed.
3. Per-agent sessions, journals, rollouts, and recovery.
4. Fanout with schema/model/effort/agentType options and dead-to-null behavior.
5. Exact hard-budget ceiling and no later spawn.
6. Longest-prefix resume, no duplicate fixture hits, identical budget.
7. No-barrier pipeline behavior.
8. Deterministic worktree allocation, cwd, guards, and cleanup.
9. Equivalent normalized journals across CLI, tool, and slash entrypoints.
10. One-level nesting succeeds and second-level nesting is rejected.

Use the repository's `test-tui` guidance for interactive testing and
`remote-tests` guidance for connected app-server/exec-server OS combinations.
Cover Linux, macOS, and Windows semantics where the feature is not intentionally
platform-specific.

## Claude parity study

The installed Claude Code version observed during recon was `2.1.201`. Record the
exact version again when tests run. Public documentation does not define the
exact JavaScript workflow contract, so compare the exposed feature empirically.

Use minimal disposable equivalents of the same scenarios. If account access,
feature availability, or material paid usage blocks execution, report that
explicitly rather than inventing results.

Compare:

- authoring and discovery;
- metadata and phases;
- globals and return types;
- agent options and structured outputs;
- parallel and pipeline timing;
- concurrency, queues, lifetime, and item caps;
- failure, skip, and `null` behavior;
- budgets and deterministic-runtime restrictions;
- resume and changed-prefix behavior;
- nesting and reload;
- CLI, tool, and slash entrypoints;
- background monitoring and redraw;
- drill-in and return;
- session persistence;
- worktree isolation and cleanup;
- completion signaling; and
- errors, exit status, and diagnostics.

The parity matrix has these columns:

| Scenario | Claude evidence | Codex evidence | Classification | Notes/follow-up |
| --- | --- | --- | --- | --- |

Classifications are `Exact`, `Behaviorally Equivalent`, `Intentional Divergence`,
or `Missing`. Do not claim parity complete while a required row is missing or
unexamined.

## Validation rules

Obey `AGENTS.md` throughout. In particular:

- never change the sandbox environment-variable behavior forbidden by the repo;
- preserve unrelated dirty changes;
- use `apply_patch` for manual edits;
- do not run `cargo test` directly; use `just test`;
- run targeted crate tests for every changed project;
- add and review `insta` snapshots for every visible TUI change;
- add integration coverage for agent-logic changes;
- regenerate app-server schemas and API documentation for API changes;
- regenerate the config schema if `ConfigToml` changes;
- run `just bazel-lock-update` if Rust dependencies change;
- update Bazel compile data for new build-time file inputs;
- run `just fmt` after code changes;
- run scoped `just fix -p <project>` before finalizing a large Rust slice;
- ask before running the complete workspace `just test` required by shared
  core/protocol changes;
- audit app-server APIs, raw response events, CLI parameters, configuration, and
  rollout resume for breaking changes; and
- run final testing, context, size, breaking-change, and code reviews.

Historical direct-`cargo` results are not final gates.

## GitHub and upstream delivery

Maintain three distinct deliverables.

### Fork-complete implementation

- Full behavior, tests, UAT evidence, and known divergences.
- Small, reviewable local commits.
- No unexplained dirty files.

### Fork tracker and pull requests

- Accurate milestone and issue state.
- Evidence linked from the relevant issue.
- Reviewable pull-request series instead of expanding the existing 25K-line PR.

### OpenAI upstream proposal

- Reconcile with `openai/codex#24721` and `openai/codex#25446`.
- Never submit the current branch as one upstream PR.
- Start with the smallest independently valuable, maintainer-aligned foundation.
- Keep optional runtime and TUI work in later stages.
- Seek maintainer alignment before implementation PRs.
- Do not treat an open proposal issue as acceptance.

## Completion criteria

The project is complete only when:

- forensic objects are preserved;
- the default production-host path and agent fanout genuinely work;
- M0 through M4 behavior is implemented, or a scope reduction is explicitly
  approved;
- UAT-1 through UAT-10 pass deterministic gates;
- an agent completes real human-style TUI testing;
- an independent judge report exists;
- the Claude parity matrix is evidence-backed and complete;
- targeted tests, schemas, snapshots, formatting, and lints pass;
- the full workspace result is available if authorized;
- breaking-change and final code reviews are complete;
- no unexplained worktree changes remain; and
- the fork series and upstream decomposition are ready for review.

External Claude access or upstream maintainer alignment may remain an external
dependency. Finish all local implementation and evidence that does not depend on
it, then report the dependency precisely without claiming parity signoff or
upstream acceptance.
