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
- execution through either the effective native Codex provider or an existing
  configured router/provider such as Headroom, without child model overrides
  silently resetting provider configuration;
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
  [#30–#48](https://github.com/kedarkolluri/codex/issues/30), plus provider/router
  compatibility in [#58](https://github.com/kedarkolluri/codex/issues/58),
  model-context bounds in [#59](https://github.com/kedarkolluri/codex/issues/59),
  and runtime progress emission in
  [#60](https://github.com/kedarkolluri/codex/issues/60). The control rescue is
  tracked as safe script save [#61](https://github.com/kedarkolluri/codex/issues/61),
  whole-run stop [#62](https://github.com/kedarkolluri/codex/issues/62),
  pause/resume [#63](https://github.com/kedarkolluri/codex/issues/63), and
  selected-agent controls [#64](https://github.com/kedarkolluri/codex/issues/64).
- [#25](https://github.com/kedarkolluri/codex/issues/25) owns deterministic tests,
  human-agent UAT, cross-host conformance, CI, and Claude comparison. Its
  children are [#49–#55](https://github.com/kedarkolluri/codex/issues/49) and
  [#57](https://github.com/kedarkolluri/codex/issues/57), plus control-parity
  UAT [#65](https://github.com/kedarkolluri/codex/issues/65).
- [#56](https://github.com/kedarkolluri/codex/issues/56) owns the rebase-friendly
  architecture audit and staged upstream strategy.

Immediate critical path:

1. Finish the final-review context hardening, targeted tests, and schema refresh.
2. Rebuild and repeat final-hash Codex human TUI evidence; retain the already
   authenticated Claude 2.1.201 comparison and independent judgments under #65.
3. Reconcile #24, #25, #56, and control children #61–#65 with the final evidence
   and explicit external platform/startup-recovery test gaps.
4. Run final scoped fixes and formatting, then capture the exact dirty tree
   under a rescue ref and verified bundle without touching the real index.
5. Rehearse the dependency-ordered semantic rebase onto current OpenAI main in
   an isolated disposable checkout and update #56's staged upstream proposal.
6. Record exact external platform blockers for gates that cannot run on this
   host; do not convert them into passes.

## Recovered repository state

Snapshot taken on 2026-07-18:

- repository: the current repository root (`./`);
- branch: `claude/dynamic-workflows-impl`;
- HEAD: `561b4dadee915285adc11efd2e9f6296c83989fc`;
- HEAD matches `origin/claude/dynamic-workflows-impl`;
- only the `kedarkolluri/codex` fork remote is configured, while current OpenAI
  `main` is preserved locally at `refs/remotes/upstream/main` without changing
  the user's remote configuration;
- the branch is 42 commits ahead of fork `main`;
- the active rescue tree has 132 modified tracked paths and 127 untracked paths,
  with the tracked diff currently around 7,875 insertions and 7,361 deletions;
  the deletion count is inflated by behavior-preserving module extraction into
  untracked sibling files;
  and
- it is 49 commits ahead and 199 commits behind the captured OpenAI `main`
  (`643de86a1`), with merge base `c888e8e75`.

This is much too large and divergent for one upstream pull request.

Live upstream refresh on 2026-07-19: OpenAI `main` is
`678157acaa819d5510adfe359abb5d0392cfe461`, preserved without changing the
configured remotes at
`refs/rescue/dynamic-workflows/upstream-main-20260719-678157ac`. The committed
branch is 49 commits ahead and 216 commits behind that pin, with merge base
`c888e8e75a9f0e90ce7d5517f8b9540832cbbf76`. Final dirty-tree counts and a new
rescue checkpoint remain deliberately deferred until hardening, formatting,
and final-hash validation finish.

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
  journal storage/indexing, and prefix-replay resume. The default process-owned
  host, real agent fanout, native-provider inheritance, and configured-router
  path now have real-stack coverage.
- M4 now has durable background lifecycle, CLI `run`/`ls`/live `watch`, the
  saved-only model tool, app-server notifications/replay, deterministic worktree
  isolation, TUI picker/monitor, child drill-in/return, completion signaling,
  startup recovery, and process-interruption resume. The monitor now uses
  distinct compact UUIDv7 identifiers, bounded full/compact/overflow retention,
  and event-timestamp elapsed/duration rendering. Workflow feature toggles are
  persisted as restart-required changes instead of pretending to reconfigure
  the already-running app-server. Run-targeted cancellation and experimental
  app-server `workflow/stop` now atomically join one cleanup path. The TUI now
  requires explicit retained full-run focus, hides Stop on compact/saturated or
  inactive runs, defaults confirmation to Cancel, suppresses duplicates while
  pending, and renders typed outcomes. Final-hash human UAT proved whole-run
  Stop, project/personal script-only Save, durable Pause/Resume, selected-agent
  Skip/Retry, live reload, and same-process plus cross-process resume. New,
  nested, and resumed runs persist the current root
  thread owner through `meta.json`, SQLite, rebuild, recovery, and CLI
  projection; legacy ownerless metadata stays readable and cannot silently
  acquire authority.
- MT deterministic workflow UAT and real in-process/process-host provider-router
  parity are green. Visual-only 120x36 Codex drivers plus artifact-only judges
  pass discovery, redraw, child drill/return, exact IDs/timing, Stop, Save,
  Pause/Resume, selected-agent Skip/Retry, live reload, and clean exit. Strict
  fresh-server CLI transcripts pass failure/null, budget, parallel, pipeline,
  nesting, and worktree lanes. Authenticated Claude 2.1.201 controls prove
  startup discovery, Save, immediate Stop/Pause, and explicit same-process
  resume; two final judges return PASS for Codex primary-workhorse readiness
  with no demonstrated Codex P0/P1 blocker. Exhaustive matched Claude semantics,
  full remote/Windows/macOS coverage, final review, delivery-series capture,
  and upstream reconstruction remain.

Two final review hardening changes are also implemented: child turns use an
atomic idle-admission reservation so unrelated queued input cannot capture a
workflow child, and markerless legacy `running` metadata projects as `Unknown`
rather than being guessed live or terminal from an unlocked lease.

The final model-context review additionally found and closed two fail-open
boundaries. Workflow-child ownership now persists as a dedicated thread source
and is reconstructed after registry loss/resume, so collaboration tools,
environment/list visibility, usage hints, and duplicate parent-mailbox
completion stay suppressed after restart. Role-selected instruction lanes,
the role catalog, and configurable MultiAgentV2 hints now have hard per-item and
aggregate byte caps; role application is atomic; and the standalone usage hint
is a typed `ContextualUserFragment`. Accepted model `workflow_run` calls now
have a separate 4 KiB raw/3 KiB nested-args boundary. The manual >1K-token
signoffs and invalid-model-output history policy are recorded in
`.github/DYNAMIC_WORKFLOWS_CONTEXT_AUDIT.md`.

### Rescue validation snapshot

The following gates are green in the active tree:

- all 25 workflow-focused CLI crate/integration tests, including the 17
  entrypoint-specific integration cases, the default process host, live multi-frame NDJSON
  watch, entrypoint parity, true partial resume, and a public populated
  `workflow run` → separate `workflow ls` round trip;
- all 43 app-server workflow tests, including Stop, Save, Pause/Resume,
  selected-agent controls, ownership, replay, and lifecycle behavior;
- 274/274 app-server-protocol tests and regenerated stable plus experimental
  schemas;
- 73/73 TUI workflow tests with reviewed monitor/control snapshots and no
  pending snapshot files;
- 157/157 code-mode tests and 88/88 code-mode-protocol tests;
- 67/67 core-workflows tests, including exact-script save tests covering
  bounds, validation, traversal/link/reparse defenses, atomic conflict and
  overwrite publication, concurrency, and private Unix modes;
- 108/108 workflow-journal, 168/168 state, 254/254 protocol, 34/34 git-utils,
  96/96 rollout, and 27/27 process-host tests;
- 196 focused core workflow tests, including ownership, recovery, CLI, nested
  execution, budgets, worktrees, and selected-agent controls;
- 26/26 Python fixture-manifest, mock-server, and transcript-verifier tests;
- strict fresh-server transcript verification for failure/null (3 requests),
  budget (2), parallel (3), pipeline (4), nesting (0), worktree (2), Stop (1),
  Skip (3), Retry (5), and cross-process Pause/Resume (3);
- real two-cell broker isolation across both hosts;
- persisted workflow-child ownership across registry removal and rollout
  resume, including fail-closed collaboration and no duplicate parent history;
- real in-process and process-owned workflow children omitting generic
  collaboration specs/runtimes and failing an unadvertised parent-mailbox call;
- bounded role-selected instruction lanes/catalog and bounded configurable
  MultiAgentV2 prompt payloads;
- real-stack unmetered-versus-explicit-zero budget semantics;
- local and Docker remote-executor worktree fail-closed coverage.

These are the latest focused results, not a final release certificate. Scoped
fix passes are clean for `core-workflows`, workflow-journal, state,
app-server-protocol, app-server, and TUI. `just fix -p codex-core` exits successfully
but still reports seven production and two test warnings in the broader rescue,
including large broker enum variants, adapter argument counts, and four
lock/guard-across-await findings; those warnings still require resolution or a
reviewed narrow rationale. Recent TUI and documentation work also requires one
coordinated final format/diff pass. The complete workspace `just test` was not
authorized after it was offered, so only repository-sanctioned changed-crate
and focused suites are in scope for the final local certificate.

Startup recovery has strong core/storage coverage, but app-server exposes no
workflow-run read/list/status method and recovery does not append a terminal
event to the owner rollout. Consequently, a public JSON-RPC startup-recovery
test cannot distinguish a build with reconciliation from one without it. A
reliable public E2E requires a deliberate v2 run-status endpoint or a recovered
terminal rollout/notification contract; this is recorded as a precise API/test
gap rather than papered over with a private-helper test.

A full `codex-tui` run reached 3,068 passed and 4 skipped tests; five failures
and one timeout were confined to the unrelated IDE-context IPC group. An
isolated retry failed with `PermissionDenied` because the IDE socket directory
is writable by other users, so this is recorded as a host-environment blocker,
not converted into a green full-crate claim.

The Bazel/Wine executor lane cannot run on this aarch64 host; the repository's
remote-test guidance requires an x86_64 Linux host. The lane remains an external
required gate rather than a local skip or claimed pass.
A narrow Windows-target `codex-git-utils` check is green. The broader
Windows-target core check is externally blocked because the pinned
`rusty_v8` GNU artifact URL returns HTTP 404 before workflow code can build.
No macOS runner is available locally.

### Historical dirty slice inherited from the interrupted session

Modified:

- `.claude/workflows/dw-m4-cli-entrypoint.js`
- `codex-rs/cli/src/main.rs`
- `codex-rs/core/src/lib.rs`
- `codex-rs/core/src/tools/code_mode/mod.rs`

Untracked:

- `codex-rs/cli/tests/workflow_run.rs`
- `codex-rs/core/src/tools/code_mode/cli_entry.rs`

At the initial rescue snapshot, that dirty slice attempted:

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

The problems above describe the inherited slice. The active rescue has repaired
the process-host callback bridge, real agent fanout, bounds, hostile run-ID
validation, structured errors, root/profile integration, and the missing test
coverage. Keep the historical diagnosis because it explains why the original
green smoke tests were insufficient.

## Forensic state to preserve

The former review worktree under `/tmp/claude-1000` is gone and its registration
is prunable. Its detached review snapshots are pinned under
`refs/rescue/dynamic-workflows/review/*`:

- `c6811df20`
- `ea36076e5`
- `3d21952bb`
- `1a07b6389`
- `226236fb2`
- `a611467ea`
- `4e5ac1b88`
- `4e91009bc`
- `0791ceaa1`

Two dropped stash commits are likewise pinned under
`refs/rescue/dynamic-workflows/stash/*` while `git stash list` is empty:

- `ad12fe5b4`
- `786490891`

The exact active dirty tree before the 2026-07-19 control stages is also pinned
without changing the real index:

- ref: `refs/rescue/dynamic-workflows/checkpoint/10-active-tree-20260719T073228Z`;
- commit: `736c063d606295e41eca476fc5ffdb342fd03248`;
- tree: `99cc6fbf90001eb689fc43f0cffcd4021ef5f6cb`;
- verified complete-history bundle:
  `.git/rescue-bundles/dynamic-workflows-active-tree-20260719T073228Z.bundle`;
- bundle SHA-256:
  `4128079e8719a504833f82b92dff7e5ca7ea5823bde7d5c429f14c3108a4d118`.

An alternate index contained every previously untracked path, matched the
working tree exactly, and wrote the same tree as the checkpoint commit. The
real `.git/index` SHA-256 remained
`bbfb48769cf72aecd33e1805ba1afc2d85944613b8592244dae65a20ede847d0`
before and after capture. Later control work requires a new final checkpoint;
this ref remains the immutable pre-stage recovery point.

Do not remove those rescue refs until the delivery series and bundle are safely
published. Do not cherry-pick the detached M2 snapshot wholesale; the active
branch contains later refinements.

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
- Reserve child-turn idle admission atomically so foreign queued input cannot
  steal or merge with a workflow child turn.
- Treat markerless legacy running metadata as `Unknown`; never infer liveness or
  terminal success merely from whether an advisory lease can be acquired.
- Provide run-targeted stop, checkpoint pause/resume, and safe script-only save
  with durable, idempotent acknowledgement. Implement selected-agent skip and
  retry as exact-attempt product controls without corrupting budgets, worktrees,
  aggregate accounting, or replay ordinals.

### Product entrypoints

- `codex workflow run`, plus `ls` and `watch` where defined by the plan.
- Model-callable `workflow_run`.
- Interactive `/workflow` picker and launcher.
- Normalize all entrypoints to equivalent outcomes and journals.
- Feature-gate every surface consistently.
- Inherit the effective model provider, authentication, endpoint, headers, and
  profile configuration. Applying per-agent model/effort overrides must not
  reset the provider or bypass a configured router.
- Prove the same fixture workflow under the native/default provider configuration
  and a local mock OpenAI-compatible router without live network or secrets.

### App-server and TUI

- Add the necessary `Workflow*` protocol events.
- Add v2 app-server `workflow/*` notifications and mappings.
- Add experimental thread-owned workflow control methods without leaking whether
  a run belongs to another thread.
- Model workflow run, phase, and agent state explicitly.
- Render a `WorkflowProgressCell` with pending skeletons and
  pending-to-active-to-done transitions.
- Redraw progress in place instead of appending noisy duplicates.
- Let a user drill into a running child, observe streamed activity, and return to
  the live monitor without losing the parent subscription.
- Keep child sessions inspectable after completion.
- Emit a completion notification.
- Render distinct concurrent run identifiers, bounded compact overflow cards,
  explicit saturation, and run/phase elapsed or duration derived only from
  app-server event timestamps.
- Persist `/experimental` workflow toggles as restart-required configuration;
  the running app and app-server keep their immutable startup capability.
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
  parity. The PTY lane need not be a merge-blocking CI job, but a fresh driver
  report and independent verdict are required release evidence before parity
  signoff.
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
exact version again when tests run. Anthropic's current documentation defines a
JavaScript workflow contract but describes the current product, not necessarily
every 2.1.201 detail, so compare the installed feature empirically and keep
documented versus observed evidence separate.

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

### Captured upstream audit

A read-only three-way audit against captured OpenAI `main` (`643de86a1`) found
49 branch commits ahead and 199 behind, with 47 committed and 65 dirty tracked
paths also changed upstream. Exact committed-layer conflicts appear in the
Bazel/Cargo locks, generated server notification TypeScript, code-mode globals,
agent control, session wiring, and feature tests.

The current tree must not be resolved with whole-file `ours`: that would drop
newer upstream audio/image/yield behavior, feature/config additions, session and
history semantics, app-server environment notifications, and TUI navigation.
Reconstruct the workflow stack semantically on top of upstream after capturing
the active tree as a dependency-ordered series. Regenerate locks and schemas
from merged source instead of hand-merging generated output.

The recommended product stack is ordered by feature/config, metadata/loading,
protocol/run model/budget, journal/state, IPC/process host, V8 runtime,
scheduling/delegation/worktrees, lifecycle/recovery, entrypoints, app-server,
TUI, and UAT. Keep rescue/parity evidence, general product plans, and
`.claude/workflows/**` on the fork/tracker branch rather than mixing them into
upstream product PRs.

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
- whole-run stop, checkpoint pause/resume, script-only save, and selected-agent
  skip/retry are implemented and UAT-proven without substituting a whole-run
  restart or a paper design for the selected-attempt controls;
- UAT-1 through UAT-10 pass deterministic gates;
- an agent completes real human-style TUI testing;
- an independent judge report exists;
- the Claude parity matrix is evidence-backed and complete;
- compatible Linux/Docker, x86_64 Wine/Windows, and macOS gates have evidence or
  are named as precise external blockers;
- targeted tests, schemas, snapshots, formatting, and lints pass;
- the full workspace result is available if authorized;
- breaking-change and final code reviews are complete;
- no unexplained worktree changes remain; and
- a rescue ref/bundle, isolated semantic rebase rehearsal, reviewable fork
  series, upstream decomposition, and enumerated evidence manifest are ready.

External Claude access or upstream maintainer alignment may remain an external
dependency. Finish all local implementation and evidence that does not depend on
it, then report the dependency precisely without claiming parity signoff or
upstream acceptance.

## Final `/goal` prompt

```text
/goal Continue the Dynamic Workflows rescue from the current repository root (`./`) until this fork is a production-quality daily workhorse that can replace Claude Code for normal use. Work only in this repository or explicit disposable `/tmp` test areas. Names such as `/root/<task>` are collaboration-agent identifiers, never filesystem destinations. Preserve every user edit, the inherited `.claude/workflows/dw-m4-cli-entrypoint.js` change, registered worktree artifact, dropped-stash object, and `refs/rescue/dynamic-workflows/*` ref. Do not clean, rebase, or resolve the dirty primary worktree destructively. Obey `AGENTS.md`, use small adapter-shaped modules and reviewable stages, and keep the work easy to replay on moving OpenAI upstream.

Keep [epic #1](https://github.com/kedarkolluri/codex/issues/1), [Project #3](https://github.com/users/kedarkolluri/projects/3), and their real sub-issue hierarchy current as evidence changes. In particular, track safe save in #61, run stop in #62, pause/resume in #63, selected-agent controls in #64, and control UAT in #65; do not close or mark a row done merely because its internal primitive exists while its authenticated product surface or evidence is missing.

Finish and verify the runtime end to end through both in-process and process-owned hosts. Every entrypoint and child must inherit the effective provider/router identity, endpoint, authentication, safe headers, profile, model, effort, collaboration mode, and role unless a documented child override changes only its intended field; prove the native configuration and a local mock OpenAI-compatible router such as the Headroom shape without live secrets. Preserve deterministic source-order workflow events across parallel, pipeline, and nested cells. Make child-turn admission atomic so queued foreign input can never capture or steer a workflow child. Treat the append-only journal as replay authority, use canonical inputs plus execution fingerprints, propagate persistence failures, and make fresh, resumed, terminal, reconnect, crash, and restart behavior explicit. A markerless legacy running record with no terminal proof is `Unknown`, not guessed completed, failed, or live from an unlocked lease. Enforce cancellation-safe run-local hierarchical budgets for parents, children, and siblings without mutating session-global settings, with identical live/replay JavaScript snapshots in both hosts.

Complete all product surfaces consistently: CLI `workflow run <name|path> [--resume <runId>]`, `workflow ls`, bounded `progress.json`-polling `workflow watch <runId> [--json]`, the saved-name-only `workflow_run` model tool, and the `/workflow` picker/background monitor. Keep the workflow feature's runtime capability immutable for the process: `/experimental` may persist an enable/disable target but must say restart is required. The monitor must show distinct concurrent UUIDv7 IDs, bounded full and compact cards, explicit overflow/saturation, pending→active→terminal phases, real counters, and event-supplied Unix-second elapsed/duration without inventing wall-clock values. Preserve drill into a bound child and return to the subscribed parent. Add thread-owned whole-run stop with idempotent cleanup acknowledgement and a distinct durable `Stopped` projection, checkpoint pause/resume from the exact persisted script and args/fingerprint, and safe script-only save to Codex project/personal roots with explicit overwrite confirmation. Persist bounded private invocation data without exposing args or secrets through discovery, notifications, logs, or model-visible context; legacy ownerless runs must remain readable but not controllable. Implement selected-agent skip and retry end to end through authenticated core, app-server, and TUI surfaces, with separately tested exact-attempt targeting, cleanup barriers, retry limits, journal ordinals, aggregate accounting, scheduler budgets, and worktree identity; a paper design is not completion. Do not mislabel Claude's selected-agent restart as whole-workflow restart. Keep detached CLI watch read-only unless an authenticated control daemon is deliberately designed. Retain real guarded worktree allocation, cwd isolation, cleanup, durable ownership, and recovery.

Prove the result in three evidence planes. Plane A is deterministic real-stack UAT-1 through UAT-10 plus Control-UAT with named whole-object assertions, focused unit/integration/snapshot tests, cancellation and completion races, lifecycle/recovery, provider/router/host parity, exact budget and replay behavior, and CLI/NDJSON twins for every applicable interactive scenario. Every accepted fresh-server lane must first pass `fixture_manifest.py --check` and then pass the fail-closed `transcript_verifier.py` for its exact canonical lane; never trim an impure transcript into a pass. Plane B is a fresh genuine built-Codex TUI pass at a fixed terminal size with `RUST_LOG=trace` and a disposable `log_dir`: a driver agent may use only visible frames and keystrokes (send text and Enter separately), and a different no-context judge must evaluate preserved frames, transcript, dimensions, timing checkpoints, exit status, and sanitized trace evidence. Exercise discovery, start, multi-agent progress, distinct IDs, elapsed/duration, overflow, drill/return, restart-only feature messaging, stop, pause/resume, save/overwrite, selected-agent skip/retry, failure/null, budget, resume, nesting, worktree isolation, and clean exit—not one composite happy path standing in for all UAT. Plane C repeats equivalent disposable workflows against the installed Claude build with a separate driver and judge. Record the fresh Claude version, executable hash, fixture hash, exact actions, observations, and inconclusive cases. This goal authorizes harmless bounded runs against the user's already-installed Claude subscription for parity evidence; do not incur separately metered API spend, make destructive probes, or broaden fixture access without explicit authority.

Maintain a row-complete Claude comparison covering discovery, metadata, return/null/schema/options, parallel and pipeline timing, caps, budgets, deterministic restrictions, replay/divergence, nesting, hot reload, entrypoints, monitor, drill, persistence, worktrees, signaling, whole-run stop, checkpoint pause/resume, selected-agent stop/skip, selected-agent retry, script save, and cross-session behavior. Classify only as Exact, Behaviorally Equivalent, Intentional Divergence, or Missing; Pending is unfinished evidence, and parity signoff is forbidden while any required row is Pending or Missing. Reconfirm the two open upstream proposals before delivery, but do not treat them as acceptance.

Run repository-sanctioned targeted suites for every changed crate, stable→experimental→stable app-server schema generation, config schema generation if applicable, exact snapshot review with no pending files, scoped `just fix`, final `just fmt`, argument-comment lint where appropriate, breaking-change/context/testing/final code review, and `git diff --check`. Ask before the complete workspace `just test`. Obtain compatible Docker/Linux remote-executor evidence; run the Bazel Wine/Windows lane on x86_64 Linux rather than claiming an aarch64 skip; obtain macOS evidence for non-OS-specific behavior. Capture the exact dirty tree under a new rescue ref and verified bundle using an alternate index without changing the real index. Reconstruct and semantically rebase the dependency-ordered delivery series in an isolated clone/worktree on current upstream—never use whole-file `ours` for conflicted central files—and regenerate locks/schemas from merged source.

Before stopping, publish an enumerated evidence manifest containing: repository/branch/HEAD/divergence; worktrees and rescue refs; GitHub epic/project/upstream issue state; changed-file and commit-series map; provider/router/host matrix; targeted and authorized workspace test counts; schema/config/snapshot/lint/fmt results; UAT-1..10 artifacts and separate driver/judge verdicts; Claude version/hash and completed parity matrix; Linux/Windows/macOS results or exact external blockers; breaking-change/context/security review; rescue ref/bundle verification; and isolated rebase rehearsal result. Stop only when every charter criterion is implemented and evidenced, or when all safe local work is exhausted and a precise external blocker remains. In that case report the exact blocked criterion, commands/artifacts, and minimum external action without claiming completion.
```
