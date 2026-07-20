# Codex Dynamic Workflows final-hash TUI UAT — 2026-07-19

## Result

Final driver verdicts:

| Lane | Verdict |
|---|---|
| Baseline, monitor, and drill | **PASS** |
| Whole-run Stop | Terminal behavior **PASS**; transient request-pending frame and retained-other-run visual proof **INCONCLUSIVE** |
| Save | **PASS** |
| Selected-agent Skip | **PASS** |
| Selected-agent Retry | **PASS** |
| Cross-process Pause/Resume | **PASS** |
| Same-process live reload | **PASS**; exact requested final-text visibility **INCONCLUSIVE** |
| Same-process Pause/Resume | **PASS** |

An initial artifact-only judge returned **PARTIAL** because the first packet did
not yet prove live reload or same-process resume; it also noted that the Stop
request-pending frame was not observed. It identified no product defect. After
the live-reload and same-process-resume supplement, two independent judges
re-evaluated the frozen packet and both returned overall **PASS**. Both judged
live reload `CODEX_ONLY_PROVEN`, same-process resume `CODEX_STRONGER`, and Codex
primary readiness `CODEX_STRONGER`; neither found a demonstrated Codex P0/P1
blocker. The transient Stop frame remains an explicit evidence caveat rather
than a closed observation.

## Frozen subject and black-box method

- Branch: `claude/dynamic-workflows-impl`
- Base HEAD: `561b4dadee915285adc11efd2e9f6296c83989fc`, plus the active rescue tree
- Source-built Codex SHA-256:
  `e67bb201a6297920dc7ecd292e43b2cdbedccaff4301518cf6935283f7e5d7fe`
- Source-built code-mode host SHA-256:
  `385a2da62d291ecca685b1b1522494e9e32c54007475c67e9130809c31306e29`
- Deterministic loopback mock SHA-256:
  `3b66e35660c0249b9438a896c0ffd4f40af48db6591a6ecf75d05d8dbd7b6f7c`
- Canonical fixture manifest SHA-256:
  `c5f2ea325c29edb5f7a3b8a79deb060f3ee733593c1a78a0e2783ada2f479724`
- Fail-closed transcript verifier SHA-256:
  `9a5ff37e16b4c212c2cfb849338337740b63e45d7d2950452271b34dc9d1bfee`
- Terminal geometry: fixed `120x36`
- Launch: genuine `RUST_LOG=trace just codex`, source host, loopback provider,
  disposable standalone committed Git project, and disposable homes, SQLite,
  and logs

Independent drivers could use only their assigned tmux pane and visible
keystrokes. Text and Enter were separate actions. They could not inspect source,
files, logs, processes, network state, credentials, controller metadata, or
prior reports. Controller checks ran after driver exit or at explicitly
non-mutating observation points.

Raw transcripts remain disposable and are not repository artifacts:

- Final multi-lane run: `/tmp/codex-dw-tui-final.ez1Bgr`
- Narrow independent Stop repetition: `/tmp/codex-dw-stop-pending.mSDww0`
- Supplemental live-reload and same-process-resume run:
  `/tmp/codex-dw-reload-resume.ORFHEl`

## Strict transcript evidence

The fail-closed verifier checked the loopback startup record, sequential request
IDs, exact route count and multiset, zero rejected requests, forbidden-route
absence, and the required staggered-pipeline ordering.

| Lane | Exact accepted requests |
|---|---:|
| Baseline/monitor | 4: one tool and one final request for each of two agents |
| Stop | 1: `stop/tool`; no final request reached the provider |
| Save | 0 in each signoff lane |
| Skip | 3: selected child tool; sibling tool and final |
| Retry | 5: selected child tool twice and final; sibling tool and final |
| Cross-process Pause/Resume | 3: pre-pause tool; resumed tool and final |
| Failure/null/schema/options | 3 |
| Exact budget 36 | 2 |
| Parallel barrier | 3 |
| Staggered pipeline | 4 |
| Nesting depth guard | 0 |
| Worktree isolation | 2 |

The narrow Stop repetition independently preserved the one-tool/no-final route
shape. The supplemental same-process transcript contained an earlier naturally
completed pause run and therefore is not claimed as an isolated strict pass. Its
exact order was `pause/tool, pause/final, pause/tool, pause/tool, pause/final`;
the controlled paused/resumed pair is the final three requests. The separate
cross-process lane above remains the canonical strict three-request PASS.

## Visible behavior and durable checks

### Baseline, monitor, and drill

The picker showed workflow name, description, scope, and phase count. A
zero-agent card remained terminal and displayed an unmetered zero budget. The
two-agent monitor progressed from 0/2 live to 2/2 completed, child drill opened
the exact selected request, Escape returned to the same selected parent, and
two concurrent zero-agent runs showed distinct compact IDs. The monitor used
the corrected lower-bound active timing labels (`elapsed ≥ X`). Clean exit and
clean fixture Git state passed.

### Stop

Only full-run focus exposed Stop. Confirmation defaulted to Cancel and accepting
that default kept the same run live. Explicit Stop produced terminal `stopped`,
typed `stop cancellation applied`, and an `interrupted · null` child. The Stop
footer disappeared after settlement and a later `x` was inert. Durable metadata,
progress, and SQLite agreed on `stopped`; no natural-completion marker appeared.

The driver could not capture a distinct request-pending frame: an atomic capture
50 ms after confirmation landed during a blank repaint and the next frame was
already terminal. The narrow Stop run reproduced the same terminal behavior but
did not close that transient visual gap. This is an evidence caveat, not a
demonstrated cancellation or durability defect.

The final visual Stop lane did not retain a second simultaneously active run,
so CONTROL_UAT's separate visual assertion that another retained run remains
unaffected is also INCONCLUSIVE. Thread/run targeting and wrong-thread isolation
pass deterministic core and app-server tests; this report does not substitute
those tests for the missing human-style frame.

### Save

Project and personal creation, typed conflict, Cancel-default confirmation,
cancel, and explicit overwrite passed. Source, durable run script, project
target, and personal target all had SHA-256
`de41e246f8bddbd319e4f92a72f03bb224b5a0c48f3909bc4f7ae42ef2367cd7`.
Personal `.agents`, `workflows`, and target modes were `0700`, `0700`, and
`0600`. Run directories and private files were `0700` and `0600`. Only exact
`script.js` was published; invocation, results, journals, transcripts, and
Claude-specific content were not copied.

One discarded harness attempt pre-created the personal root with unsafe default
directory permissions. Save failed closed twice. The corrected private-root
rerun is the PASS evidence; the discarded attempt is security diagnostic
evidence and carries no product-failure verdict.

### Selected Skip and Retry

Skip confirmation defaulted to Cancel. Explicit Skip settled only the selected
child as `shutdown · attempt 1 · user skip · null`; the sibling continued,
the same outer run completed, and terminal controls were suppressed. An earlier
attempt raced natural completion and was correctly rejected as stale; it was
discarded before the fresh PASS rerun.

Retry confirmation also defaulted to Cancel. Explicit Retry moved only the
selected child from attempt 1 to attempt 2 in the same outer run, preserved the
sibling, accumulated tool/token lineage, and completed on attempt 2.

### Cross-process Pause/Resume

Pause confirmation defaulted to Cancel. Explicit Pause published a durable
checkpoint, left the predecessor `paused`, interrupted its live child with a
null result, and exposed Resume. After driver exit, the controller changed only
the disposable mutable registry to a poison workflow and launched
`just codex resume <exact-owner-thread>` with the same project, homes, and
provider. No arguments were re-entered.

The exact paused run produced a fresh successor, linked both directions. The
predecessor remained `paused`, the successor completed, both retained the same
authorized owner, and the successor's `resumed_from_run_id` exactly identified
the predecessor. Original and successor durable scripts both had SHA-256
`ee479935c532ae122dd55fb3a8102f4173ae3eb8d72a58093fd72d60c285e691`;
their canonical invocations both had SHA-256
`74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b`.
The changed registry had a different hash, and its poison marker never appeared.

### Same-process live reload and Resume supplement

Before publication, the live picker had no `uat-hot-reload` match. It discovered
a newly published workflow without restart. The controller's first disposable
body mistakenly used Claude's invalid top-level `return` syntax; Codex visibly
rejected it with `SyntaxError: Illegal return statement`. This was a harness
authoring mistake, not a product defect. After changing only that disposable
source to valid `text()` syntax, the same Codex process executed it, then
discovered and executed a further edit. The final durable script exactly matched
the live registry source at SHA-256
`ede74ddb0fc1d58d9c3f37dfbc450f4d1281ae5e493ed620cecd4498109db078`.
The monitor did not render the requested final `UAT_HOT_RELOAD_OK` text, so only
that exact text observation remains INCONCLUSIVE; discovery and execution of
changed source passed.

In that same still-running process, `r` on a paused run created and completed a
fresh successor with visible predecessor/successor lineage and the exact
`resumed_from_run_id`. Both runs used the same owner and retained the same script
and invocation hashes as the strict cross-process lane. Every run directory was
`0700` and every private file was `0600`.

## Final judgment caveats

Both final independent judges treated these as evidence limitations, not
demonstrated product defects:

- the Stop request-pending visual transition and retained-other-run comparison
  were not captured;
- the exact final hot-reload marker was not visible on the last card; and
- the supplemental same-process transcript was not an isolated three-request
  lane.

The strict cross-process transcript, immutable replay hashes, terminal control
states, private modes, and separate fail-closed semantic lanes all passed.
