# Claude Code controls parity — 2026-07-19

## Result

The authenticated black-box driver returned:

| Capability | Driver result |
|---|---|
| Startup workflow discovery | **PASS** |
| Live workflow reload | Unsupported until process restart |
| Whole-run Stop | **PASS**, with no confirmation or Cancel path |
| Pause | **PASS**, with no confirmation or Cancel path |
| Pressing `p` to resume | Surfaced an exact invocation; did not itself resume |
| Submitting the exact invocation | Resume **PASS** |
| Project Save conflict/cancel/overwrite | **PASS** |
| Selected-agent controls | **INCONCLUSIVE** because the available child completed too quickly |

After the Codex same-process supplement, two independent parity judges both
returned overall **PASS**, found no demonstrated Codex P0/P1 blocker, and judged
Codex primary readiness `CODEX_STRONGER`. This verdict is readiness evidence for
the frozen Codex implementation; it is not a claim that every Claude comparator
row was exhaustively exercised.

## Frozen subject and black-box method

- Installed executable: `~/.npm-global/bin/claude`
- Version: `2.1.201 (Claude Code)`
- Resolved executable SHA-256:
  `86b2eab34d382c7b428fc2e9f4c97f04e46805e950582472a13eb7d48de60516`
- Terminal geometry: fixed `120x36`
- Disposable raw-artifact root: `/tmp/claude-dw-parity-final.fY0h4P`

The run used the installed product through an authenticated session. The
independent driver was restricted to visible tmux output and keystrokes and
could not inspect source, files, logs, durable state, user configuration,
processes, controller scripts, prior reports, identity, or credentials. Text and
Enter were separate actions. Controller-side durable checks occurred after the
visual driver exited. This report intentionally excludes account identity and
authentication material.

## Observed behavior

### Startup discovery and live reload

At startup, Claude discovered the disposable `/uat-hang` command and described
it as a dynamic workflow. Adding that workflow during the preceding live process
did not add the slash command; restarting the process was required. Startup
discovery therefore passed, while live hot reload was unsupported in the frozen
installed version.

### Whole-run Stop

A deterministic hold remained running and displayed `x stop workflow`,
`p pause`, `esc back`, and `s save`. Pressing `x` stopped immediately. There was
no confirmation dialog, destructive Cancel default, or cancellation path. The
controls disappeared, chat showed `Stopped`, the run later appeared terminal,
and the unexpected completion marker never appeared. The durable record was
`killed` after exit.

The shipped Stop source and the explicitly overwritten project target matched
exactly at SHA-256
`1d944e00fcdbcee83cd2c95dd63d51a309e11ea8be2885cd7ef7ee83eca933fe`.

### Pause and Resume

Pressing `p` on a second live hold paused immediately, again without a
confirmation or Cancel path. The footer changed to `p resume`. Pressing `p`
while paused did not directly resume; it returned to chat with an exact
`Workflow({scriptPath: ..., resumeFromRunId: ...})` invocation. Reopening the
monitor still showed the original run paused.

Submitting that exact visible invocation unchanged and accepting its one-time
run approval produced `Workflow resumed successfully`. The monitor returned to
`running` with Stop/Pause controls. Exiting Claude with `Exit anyway` while that
run was live left its durable record `killed`.

The deterministic hold source and both durable copied scripts matched exactly
at SHA-256
`11baf85fc97b1b3032914a1b2b8231a7007c4c09e339e25a761afeec36f4003c`.
Cross-process resume was not tested.

### Save

A completed run exposed `s save`. Project and User scopes were visible and
could be toggled. A pre-existing project target produced a typed conflict
requiring a second Enter. Escape canceled the first conflict; repeating Save and
explicitly confirming overwrite updated the project target, whose hash remained
exactly equal to the shipped source hash above. User scope was observed but was
not written, so personal-save parity is not claimed.

### Unproven comparator lanes

The shipped live child completed before selected Skip/Retry could be exercised,
so the selected-agent row is INCONCLUSIVE rather than a failure. This session
also did not run fresh Claude equivalents of Codex's exact budget enforcement,
strict failure/null preservation, staggered parallel and pipeline barriers,
nesting limit, isolated worktree cleanup, or transcript-level request
accounting. Those remain Claude evidence gaps. No result here converts an
untested comparator lane into a Claude defect.

## Final parity judgments

Two independent judges re-evaluated the same sanitized evidence after the Codex
supplement. Each returned:

- overall **PASS**;
- no demonstrated Codex P0/P1 blocker;
- Codex live reload `CODEX_ONLY_PROVEN`;
- Codex same-process resume `CODEX_STRONGER`; and
- Codex primary-workhorse readiness `CODEX_STRONGER`.

Both preserved the comparator gaps above and avoided claiming exhaustive Claude
coverage.
