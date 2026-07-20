# Dynamic Workflows Claude parity evidence

This report records the empirical Claude Code baseline used to evaluate Codex
Dynamic Workflows. It separates direct observations from public documentation,
records the completed primary-readiness comparison, and leaves unmatched
Claude rows explicitly `Missing` rather than inferring behavior.

Snapshot dates: 2026-07-18 baseline; 2026-07-19 authenticated controls and
final-hash Codex comparison.

## Installed Claude baseline

- Executable:
  `~/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`
- Version: `2.1.201 (Claude Code)`
- Package version: `2.1.201`
- SHA-256:
  `86b2eab34d382c7b428fc2e9f4c97f04e46805e950582472a13eb7d48de60516`
- Embedded build timestamp: `2026-07-03T19:53:38Z`
- Embedded Git SHA: `5bb45156ece6b12214696c88adec695b2dca1338`

The interrupted local Claude session used the feature rather than merely
containing dormant binary strings: its recovered transcript contains 14
`Workflow` tool calls using `scriptPath`. Thirteen returned a background-launch
result and one was rejected at parse time for invalid JavaScript. Thirteen
persisted workflow-run directories contain 82 agent-start records and 80 result
records; eleven runs pair every start with a result and two runs each lack one
result. Those aggregate journal counts do not establish why the two results are
missing.

The repository contains 13 saved `.claude/workflows/*.js` artifacts. Every file
starts with `export const meta`, and all directly call `agent`, `parallel`,
`phase`, and `log`. Several scripts mention `pipeline` and nested `workflow()`
inside comments or agent prompts, but none provides direct execution evidence
for either primitive.

Those `.claude/workflows` files are Claude fixtures or inherited orchestration,
not a Codex discovery root. Codex project workflows live under
`.codex/workflows`; its personal and Codex-home roots remain separate. Matched
UAT fixtures must preserve that distinction instead of making either product
silently consume the other's directory.

## Local black-box and installed-artifact evidence

The shipped SDK type declaration defines a `Workflow` tool whose input supports
`script`, `name`, `scriptPath`, `args`, and `resumeFromRunId`, and whose output
contains background task/run identifiers. Installed binary schemas and strings
also expose:

- project and user `.claude/workflows/` discovery;
- `/workflows`, `/deep-research`, and interactive `ultracode` triggers;
- `agent`, `parallel`, `pipeline`, `phase`, and `log`;
- deterministic restrictions around `Date.now`, `new Date()`,
  `Math.random`, and dynamic `import()`;
- agent worktree isolation and nested named workflows; and
- an undocumented plugin-manifest `workflows` field.

These commands were exercised without a model call:

```bash
command -v claude
readlink -f "$(command -v claude)"
claude --version
sha256sum ~/.npm-global/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe
claude --help
claude plugin --help
claude agents --help
claude project --help
claude auth status --json
claude plugin list --json
claude --effort ultracode --version
claude -p --tools '' --setting-sources project '/workflows'
```

The original isolated authentication check reported `loggedIn: false`, and that
interactive startup stopped at the login chooser. On the final recon pass the
installed client was authenticated through its existing first-party
subscription. No credential file was changed, copied, printed into committed
evidence, or moved into a disposable home. The 2026-07-19 black-box PTY pass used
that already-installed session and records only sanitized product behavior.

That result concerns the interactive monitor command only. It does not show
that workflow execution is unavailable in print mode; Anthropic's current docs
separately document workflow execution through `claude -p`.

`claude --help` exposes no shell-level `workflow` subcommand. In particular,
Claude 2.1.201 has no equivalent of Codex's planned `workflow run`, `ls`, or
`watch` CLI commands.

`claude --effort ultracode --version` ignores `ultracode` with an unknown-value
warning and lists `low`, `medium`, `high`, `xhigh`, and `max`. Anthropic's
current docs say startup `--effort ultracode` arrived in 2.1.203, although this
installed build contains the interactive `/effort ultracode` surface.

## Documented Claude contract

Anthropic's current [Dynamic Workflows documentation](https://code.claude.com/docs/en/workflows)
and [Workflow tool reference](https://code.claude.com/docs/en/agent-sdk/typescript#workflow)
say the feature requires Claude Code 2.1.154 or later and document JavaScript
workflows with top-level `await`, project/user discovery,
`agent`/`parallel`/`pipeline`/`phase`, up to 16 concurrent and 1,000 lifetime
agents, same-session cached resume, and an interactive monitor with drill,
filter, pause/resume, restart, stop, and save controls.

Static inspection of the installed 2.1.201 artifact narrows those labels:
`p` pauses by aborting the workflow controller at a replay checkpoint, `x`
stops either the selected agent or the whole workflow depending on selection,
`r` retries the selected agent within its retry cap, and `s` copies only the
script into a saved-workflow root. There is no whole-workflow restart control.
The selected-agent controls are live-attempt controls, not post-completion
rewrites: `x` aborts the selected attempt with a skip cause and resolves that
`agent()` call as `null`; `r` aborts it with a retry cause and reruns the same
source-order node before its promise settles. The installed controller permits
five retries after the initial attempt, accumulates token, tool-call, and
duration spend from every attempt, and gives every attempt a fresh child-agent
identity while retaining the workflow node identity. Repeated user retries that
exhaust the cap fail the call rather than silently adding an unbounded attempt.
The completed PTY detail view exposed only Save because controls are
state-conditional. The later live hold exposed and exercised whole-run Stop and
Pause/Resume. Selected-attempt Skip/Retry still raced terminal.

The documentation describes the current product, not necessarily every detail
of 2.1.201. Installed-build behavior must therefore win when the two differ.
Notable version boundaries include workflow size guidance in 2.1.202 and the
large-workflow warning plus startup `--effort ultracode` in 2.1.203. Claude
2.1.201's shipped type declaration says resume is same-session only; the current
docs say exiting Claude starts a workflow fresh. The current docs also state
retrospectively that before 2.1.210 the `ultracode` keyword could trigger from
non-human-input routes such as `-p`, scheduled prompts, and relayed payloads;
that 2.1.201 behavior was not exercised during this recon.

## Authenticated 2026-07-19 control pass

A restarted installed client discovered a disposable zero-agent hold at startup.
A visual-only driver then exercised its monitor while a separate controller
verified only hashes and durable artifacts afterward.

Observed Claude 2.1.201 behavior:

- adding a workflow to an already-running process did not add its slash command;
  restart was required;
- `x` stopped a full run immediately, with no confirmation or Cancel path;
- `p` paused immediately, with no confirmation or Cancel path;
- `p resume` surfaced an exact `Workflow({scriptPath, resumeFromRunId})`
  invocation instead of directly resuming;
- submitting and approving that invocation resumed in the same process;
- exiting with that successor live left its durable record as `killed`; and
- project Save exposed Project/User scope selection, typed conflict, Escape
  cancellation, and explicit overwrite, while retaining exact source bytes.

The matched Codex final-hash pass proved safe-default Stop/Pause, project and
personal Save, selected-agent Skip/Retry, live reload, same-process and
cross-process immutable resume, strict CLI semantics, and private durable
artifacts. Two independent artifact-only re-judgments returned `PASS` for
primary-workhorse readiness and found no demonstrated Codex P0/P1 blocker.

Primary-workhorse readiness and exhaustive comparator parity are separate
gates. The former passed. Untested matched Claude rows remain `Missing`; they
are not silently promoted to parity.

## Parity matrix

The dated human-style passes are preserved under
[`dynamic-workflows-uat/evidence/`](dynamic-workflows-uat/evidence/README.md).
`Pending` is not a classification. Primary-workhorse readiness passed, while
exhaustive comparator signoff remains open wherever a row is `Missing`.

| Scenario | Claude evidence | Codex evidence | Classification | Notes/follow-up |
| --- | --- | --- | --- | --- |
| Authoring and discovery | Project workflow names appeared as `/uat-zero` and `/uat-monitor`; source review was shown before execution | The `/workflow` picker showed both project workflows with descriptions and phase counts | Behaviorally Equivalent | Personal/nested precedence remains deterministic-test evidence, not matched PTY evidence |
| Metadata and phase skeleton | Source review showed `meta`; zero-agent and two-agent phase behavior completed | Static parser is 42/42 green; picker phase count and Prepare/Finish/Inspect/Report rendering passed PTY | Behaviorally Equivalent | Malformed-diagnostic wording is not matched |
| Agent return values and death-to-null | Both matched readers returned strings; death/null not exercised | Both success and a denied-child `errored · null` path were observed; deterministic death-to-null tests are green | Missing | Exercise Claude death/skip/null explicitly |
| Structured output and model/effort/agent type | Installed scripts exercise schema/model, but matched PTY used inherited Haiku only | Provider/model/effort/role inheritance and real process-host wire assertions are green | Missing | Match schema failure and each override in Claude |
| Parallel barrier behavior | Two readers visibly completed under one parallel phase | Codex showed the same two-reader parallel topology | Missing | Staggered barrier timing and positional results still need matched measurement |
| Pipeline no-barrier behavior | Installed artifact/docs expose `pipeline`; no authenticated execution yet | Deterministic no-barrier UAT is green | Missing | Run a staggered matched fixture |
| Concurrency, queue, and lifetime caps | Current docs say 16 concurrent and 1,000 lifetime; boundaries not measured | Exact scheduler/cap boundary tests exist | Missing | Measure both installed products at cap and cap+1 |
| Item cap | Installed-build boundary unmeasured | Codex enforces 4,096 items | Missing | Classify only after a bounded Claude probe |
| Budget getters and hard ceiling | Matched monitor exposed token totals, not workflow budget getters/ceiling | Real PTY exposed and fixed unmetered-vs-zero semantics; hard-ceiling/resume tests are green | Missing | Run exact ordinal ceiling and getter fixture in Claude |
| Deterministic runtime restrictions | Installed strings/docs expose restrictions; no authenticated probe | Date/random/import/timer restriction tests are green | Missing | Execute minimal rejected scripts in Claude |
| Prefix replay after unchanged calls | Docs/type surface describe same-session cached resume; not exercised | Durable longest-prefix replay and real partial-resume CLI UAT are green | Missing | Run unchanged same-session resume in Claude |
| Changed-prefix divergence | Not exercised | Prompt/model/effort/args/provider/router/role fingerprints and mismatch coverage are green | Missing | Change each input independently in Claude |
| Nested workflows | Installed machinery/docs expose nesting; not exercised | One level succeeds; second level rejects | Missing | Measure Claude depth and diagnostic |
| Hot reload | A workflow added during a live process was not discovered until restart | The unchanged process discovered, executed, and re-executed edited source | Intentional Divergence | Codex provides live reload; Claude 2.1.201 behaved startup-only |
| CLI entrypoints | `claude --help` exposes no shell workflow subcommand | `run`, `ls`, and live/completed NDJSON `watch` pass 17 entrypoint-specific integration cases within 25 workflow-focused CLI tests | Intentional Divergence | Codex adds a headless automation surface |
| Model tool entrypoint | Workflow tool accepts inline/name/path | `workflow_run` accepts bounded saved names only | Intentional Divergence | Codex intentionally rejects model-authored source/path to bound context and authority |
| Slash/picker entrypoint | Saved names appeared directly as slash commands with visible source review | `/workflow` opened a searchable picker and launched immediately | Behaviorally Equivalent | Claude has stronger pre-run source review; Codex has one consolidated picker |
| Background monitor and redraw | `/workflows` showed active 0/2, completed list, and detail | Fixed-size PTY showed 0/2 → 1/2 → 2/2 in one retained card; independent judge PASS | Behaviorally Equivalent | Claude pending frame was too fast to capture |
| Drill into child and return | Installed 2.1.201 detail visibly advertised only select/back/save; no drill control was exposed | Bound child drill, transcript view, Escape return, and retained parent monitor independently passed | Intentional Divergence | Codex is stronger than the tested Claude build; current Claude docs may describe a newer surface |
| Session persistence | Completed runs remained inspectable in `/workflows` during the session | Completed parent and child sessions remained inspectable; durable recovery/resume tests are green | Behaviorally Equivalent | Cross-process behavior is classified separately |
| Worktree isolation and cleanup | Installed surface indicates worktree support; not exercised | Local positive and remote fail-closed worktree UAT are green | Missing | Run matched writer/cleanup fixtures |
| Completion signaling | Background notification and final list/detail were visible | Terminal card, completion notification, exact counters, and app lifecycle passed | Behaviorally Equivalent | Diagnostic wording differs |
| Whole-run stop | `x` immediately stopped a zero-agent hold; no confirmation; durable state later read `killed` | Explicit run focus, Cancel-default confirmation, typed `stopped`, interrupted child, cross-store agreement, and strict request count passed | Intentional Divergence | Codex deliberately provides safer confirmation and a distinct durable stopped state |
| Checkpoint pause/resume | `p` paused immediately; `p resume` surfaced an invocation that required manual submit and approval | Cancel-default Pause plus direct `r` created a durable linked successor in the same process; restart resume replayed exact immutable inputs | Intentional Divergence | Both resume; Codex has the safer, more direct, and cross-process-capable path |
| Selected-agent stop/skip | Installed artifact exposes selected-attempt `x`; the live fixture raced terminal | Exact selected child, sibling survival, `shutdown · user skip · null`, and strict transcript passed | Missing | Comparator evidence remains open; Codex implementation/readiness passed |
| Selected-agent retry | Installed artifact exposes bounded selected-attempt `r`; the live fixture raced terminal | Same-node attempt 1→2, fresh child, sibling isolation, accumulated spend, and strict transcript passed | Missing | Comparator evidence remains open; this is not whole-run restart |
| Script-only save | Project/User scopes, typed conflict, Escape cancellation, and project overwrite were visible | Project/personal create, conflict, Cancel-default overwrite, exact bytes, private modes, and script-only publication passed | Behaviorally Equivalent | Claude User write and privacy were deliberately not exercised |
| Cross-session resume | Installed declaration limits resume to one session; exiting a resumed live hold left durable `killed` | Exact immutable script/args/fingerprint replay survived restart and registry poisoning | Intentional Divergence | Codex deliberately provides stronger durable resume |

## Remaining exhaustive Claude probes

Use tiny disposable workflows, not the rescue orchestration scripts:

Completed across 2026-07-18 and 2026-07-19: project slash discovery, visible
source review and one-time approval, zero-agent completion, two parallel reader
agents, active and terminal `/workflows`, completed list/detail, Save conflict
and overwrite, startup-versus-live reload, whole-run Stop, Pause, exact
same-process resume invocation, exit-mid-run behavior, and clean exit.

Still required:

1. Personal and nested-project precedence plus empty-state behavior.
2. Explicit failure/death-to-null and structured-schema/options behavior.
3. Staggered parallel barrier and no-barrier pipeline timing.
4. Queue/concurrency/lifetime/item boundary probes.
5. Budget getters, exact hard ceiling, and deterministic-runtime restrictions.
6. Cached-child replay, changed-prefix invalidation, and nested workflows.
7. Selected-agent stop/skip and retry with a child that remains live.
8. Worktree isolation and cleanup.
9. Optionally test the undocumented plugin workflow manifest outside the core
   replacement-readiness gate.

The human-style parity lane must use separate PTY driver and judge agents. The
driver may rely only on visible terminal output and keyboard input. The judge
receives captured frames/events and the rubric, not internal Rust state.
