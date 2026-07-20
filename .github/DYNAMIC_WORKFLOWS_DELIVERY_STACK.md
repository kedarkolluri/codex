# Dynamic Workflows delivery stack

This is the semantic decomposition plan for turning the recovered Dynamic
Workflows tree into reviewable commits and branches. The archival rescue commit
is a disaster-recovery artifact only. It must never be reviewed, merged, or
rebased as one implementation change.

## Recovery baseline

- archival branch: `rescue/dynamic-workflows-wip-20260719`
- archival commit: `97f03385584f8685fb6ef8dd798d6cc8a25f6d57`
- archival tree: `37d0f9fb2ebd47eda71d58f364258eb24a6df0ef`
- verified bundle: `../../dynamic-workflows-wip-20260719.bundle`
- bundle SHA-256:
  `c2b868f7724ffedbe7f03f7daa865ff5aa8eb727bfa5053b9b5efabd1c6638a8`

The working tree contains behavior changes, tests, generated files, evidence,
and large module extractions. Those categories must be reconstructed in
dependency order rather than committed by filesystem area or by the order in
which the interrupted agent happened to edit them.

## Branch stack

Each branch below is based on the preceding branch unless its pull request
explicitly documents a different base. Every slice must compile and carry its
own behavioral tests before the next slice is built.

1. `dw/00-mechanical-extractions` — reconstruct behavior-neutral module moves
   from upstream first; do not copy extracted files that already contain later
   workflow behavior.
2. `dw/01-bounds-contracts` — shared bounded-input and serialization contracts.
3. `dw/02-agent-context-config` — bounded agent roles, instructions, and
   collaboration context.
4. `dw/03-workflow-child-isolation` — typed ownership, child admission, and
   generic-collaboration isolation.
5. `dw/04-workflow-event-contract` — protocol-level workflow events and stable
   identifiers.
6. `dw/05-run-model` — durable workflow-run model and state representation.
7. `dw/06-journal-format-storage` — journal format, keying, and atomic storage.
8. `dw/07-journal-replay` — bounded replay and deterministic validation.
9. `dw/08-state-projections` — SQLite/runtime projections and rebuild behavior.
10. `dw/09-run-budget` — explicit budget semantics and hard limits.
11. `dw/10-runtime-progress` — typed progress production inside the runtime.
12. `dw/11-process-host-seam` — process-owned host protocol and isolation seam.
13. `dw/12-core-dispatch-progress` — core dispatch integration and progress
    routing.
14. `dw/13-provider-router` — effective provider/router inheritance and parity.
15. `dw/14-worktree-isolation` — deterministic worktree creation and cleanup.
16. `dw/15-lifecycle-progress` — run lifecycle publication and cleanup barriers.
17. `dw/16-recovery-resume` — startup recovery and immutable prefix replay.
18. `dw/17-model-entrypoint` — saved-only model-callable `workflow_run`.
19. `dw/18-cli-entrypoints` — `workflow run`, `ls`, and live `watch`.
20. `dw/19-appserver-observability` — v2 notifications, replay, and ownership
    checks.
21. `dw/20-tui-monitor` — picker, monitor, drill-in, return, and notifications.
22. `dw/21-save-core` — safe script-only save implementation.
23. `dw/22-save-appserver` — save API and public behavioral coverage.
24. `dw/23-save-tui` — save interaction and snapshots.
25. `dw/24-stop-core` — whole-run stop and cleanup join.
26. `dw/25-stop-appserver` — stop API and lifecycle coverage.
27. `dw/26-stop-tui` — stop confirmation, outcomes, and snapshots.
28. `dw/27-pause-resume-core` — immutable pause checkpoint and resume.
29. `dw/28-pause-resume-appserver` — pause/resume API and replay coverage.
30. `dw/29-pause-resume-tui` — pause/resume interactions and snapshots.
31. `dw/30-agent-controls-core` — selected-generation skip/retry controls.
32. `dw/31-agent-controls-appserver` — selected-agent control API and events.
33. `dw/32-agent-controls-tui` — selected-agent controls and snapshots.
34. `dw/33-deterministic-uat` — deterministic black-box fixture and transcript
    lanes.
35. `dw/34-control-uat-evidence` — human-style TUI controls UAT and Claude
    comparison evidence.
36. `dw/35-tracker-docs` — final tracker reconciliation and handoff documents.

## Slicing rules

- Keep non-mechanical changes below 800 changed lines; target less than 500 for
  complex logic. Split a branch again when that limit cannot be met coherently.
- Recreate mechanical extractions from the chosen upstream base, then layer
  behavior on top. A directory move containing later behavior is not a
  mechanical commit.
- Couple migrations with the API/model change that requires them.
- Couple lockfiles with the dependency change that produced them.
- Couple every user-visible TUI change with its reviewed `insta` snapshots.
- Regenerate app-server schemas for the relevant API slice. Do not carry the
  current deletion-heavy generated diff forward. Generate stable, then
  experimental, then stable again and verify a clean final stable pass.
- Do not place UAT evidence, generated transcripts, or rescue artifacts in an
  implementation commit unless the slice explicitly owns that evidence.
- Rebase and validate each slice before stacking dependents; never resolve the
  entire rescue tree as one conflict set.

## Promotion gates

A slice is promotable only when:

1. its diff matches one semantic objective;
2. focused repository-sanctioned tests pass;
3. UI changes include reviewed snapshots;
4. public protocol changes include regenerated schemas and public API tests;
5. ownership and model-context changes have fail-closed regression coverage;
6. no generated or unrelated user changes are swept into the commit; and
7. its pull-request body identifies the next dependent slice.

The final fork release additionally requires no-context agents operating the
real TUI as human users, artifact-only independent judges, and an explicit
black-box comparison with the installed Claude Code workflow feature. Automated
fixtures alone are not a substitute for that UAT.
