# Dynamic Workflows control UAT protocol

This is the driver/judge protocol, not a result. A control row remains unproven
until a dated evidence report records a fresh run of the built product.

## Isolation and roles

- Launch the genuine source-built TUI with `RUST_LOG=trace`, `just codex`, a
  disposable `HOME`, `CODEX_HOME`, `CODEX_SQLITE_HOME`, a disposable project
  fixture, and `-c log_dir=<disposable-log-dir>`. Launch under `umask 0002` so
  private-mode verification proves product enforcement instead of inheriting a
  restrictive harness mask. When launching through `just codex`, explicitly
  point `RUSTUP_HOME` and `CARGO_HOME` at the already-installed build caches;
  isolating `HOME` must not silently install another toolchain or rebuild the
  workspace from a fresh Cargo registry.
- Copy the project fixture into a fresh standalone Git repository under the
  lane's disposable `/tmp` root and commit its baseline before launch. Never run
  the worktree lane from this source checkout or any other live repository.
- Route the control and monitor fixtures through `mock_responses_server.py`,
  bound only to loopback, so each run exercises the real model/tool pipeline
  without a paid request. Record the server's selected port and complete
  request-phase transcript. Start a fresh mock process for every lane and every
  rerun; the pipeline proof is deliberately one-shot and poisoned by duplicate
  or out-of-order requests so stale state can never become evidence.
- Build the disposable `config.toml` from
  `codex-control.config.toml.in`, replacing only `@BASE_URL@` with the exact
  loopback URL printed by the mock. The template disables auth and retries,
  selects `approval_policy = "never"`, and enables the workflow feature. It
  uses `danger-full-access` only for this disposable fixture because nested
  Linux hosts may not let Bubblewrap configure loopback; the deterministic
  router emits only host-selected bounded delays plus one fixed marker
  create/read/remove and `git status` proof inside the fresh standalone fixture.
  Never reuse this config with a live repository, live provider, or
  nondeterministic provider.
- Force and record a fixed `120x36` PTY.
- A no-context driver agent may inspect only visible terminal frames and send
  keystrokes. It must send text and Enter as separate operations.
- The driver must not inspect source, fixtures, files, processes, logs,
  journals, network state, or authentication state.
- A different no-context judge receives only the rubric, preserved frames,
  dimensions, action/timing transcript, exit status, and sanitized trace
  excerpt. It must not inspect the live process or repository.
- Preserve exact executable/version and fixture hashes. Do not reuse a verdict
  after either changes.
- Before accepting any lane, run
  `python3 .github/dynamic-workflows-uat/fixture_manifest.py --check` from the
  repository root. A stale or mismatched manifest invalidates the lane.
- After each fresh mock server exits, pass its complete stdout JSONL—not a
  filtered excerpt—through `transcript_verifier.py --lane <canonical-lane>`.
  Extra, missing, duplicate, out-of-order, unknown, or malformed requests fail
  closed. Never trim an impure transcript into an apparent pass.
- Keep raw frames, traces, journals, and transcripts only under the disposable
  `/tmp` root. Commit only sanitized reports, frame excerpts, hashes, and
  verifier results under `evidence/`.

## Discovery/monitor/drill UAT

Use `uat-zero` and `uat-monitor` before the destructive control lanes.

1. Open `/workflow` and capture both names, descriptions, scopes, and phase
   counts at the fixed geometry.
2. Start `uat-zero`; capture its exact terminal markers, retained full card,
   and unmetered budget rendering.
3. Start `uat-monitor`; capture pending/running, two distinct child rows,
   partial progress, event-supplied timing, counters, and the final `2/2` card.
4. While a child is live, drill into that exact child, capture visible streamed
   activity, then press Escape and prove the same subscribed parent card and
   selection are retained.
5. Start a second retained run and prove its compact ID is distinct. Exit
   cleanly and preserve the process exit status.

The judge fails the run for hidden-source dependence, duplicate cards instead
of in-place redraw, invented elapsed time, ambiguous run identity, loss of the
parent subscription after drill/return, or a missing/null child result being
rendered as success.

## Stop-UAT

Fixture: `uat-stop`, whose only child is instructed to run the harmless bounded
host command `sleep 120` on POSIX or `Start-Sleep -Seconds 120` on Windows and
perform no other action. The longer hold accommodates frame-by-frame no-context
drivers without changing the behavior under test.

1. Open `/workflow`, discover `uat-stop`, and start it.
2. Capture the retained running card and active `stop-target` child.
3. Enter monitor focus. Prove agent focus does not advertise or trigger
   whole-run stop.
4. Move explicitly to the full run. Prove `x stop workflow` appears only there.
5. Press `x` and capture the confirmation with **Cancel selected by default**.
6. Press Enter. Prove no stop request was emitted and the run remains active.
7. Reopen confirmation, move explicitly to **Stop selected workflow**, and
   confirm.
8. Capture pending cancellation, suppression of repeated `x`, joined cleanup,
   and the distinct terminal `stopped` state. The unexpected-natural-completion
   marker must never appear.
9. Prove another retained run is unaffected, then exit cleanly.

The judge fails the run for implicit/latest-run targeting, stop on an agent or
overflow summary, destructive default selection, duplicate request spam,
response before cleanup, `failed`/`interrupted` masquerading as user stop,
or loss of another run.

## Save-UAT

This stage runs only after `workflow/save` and its TUI are implemented.

Fixture: `uat-save`, loaded from the disposable Codex-home registry so neither
the disposable project nor personal destination exists before the first save.
The workflow has no agents and emits only fixed UAT markers.

1. Select a completed full run explicitly; no agent or compact summary may be
   used as the source.
2. Choose project scope and observe the checked-in/secret warning.
3. Save create-only and verify the typed created result.
4. Repeat to obtain a conflict without changing the target.
5. Choose overwrite, observe the second destructive confirmation with Cancel
   selected by default, then explicitly confirm overwrite.
6. Repeat in personal scope.
7. The evidence verifier—not the visual-only driver—compares exact `script.js`
   bytes and proves no args, results, journals, transcripts, or `.claude` files
   were written.

## Pause/resume-UAT

This stage runs only after the authenticated pause and resume surfaces exist.
Use a disposable workflow whose bounded child is held in `sleep 120` on POSIX
or `Start-Sleep -Seconds 120` on Windows. The visual picker lane uses its normal
`null` args; a paired deterministic app-server lane passes
`{ "marker": "resume-marker" }` so exact non-null persisted-argument reuse is
verified without exposing private invocation data to the driver.

1. Select the full running card explicitly. Agent rows and compact summaries
   must not advertise whole-run pause.
2. Open pause confirmation and prove **Cancel** is selected by default.
3. Cancel once, then reopen and explicitly confirm pause.
4. Capture pending cleanup and the durable `paused` terminal card. The response
   must not settle before the child, permit, recorder, lease, and disposable
   worktree are released.
5. Outside the visual driver, the evidence controller hashes the paused run's
   persisted `script.js` and canonical private `invocation.json`, then removes
   or replaces only the disposable registry copy of `uat-pause.js`. This makes
   a mutable-registry reread observably different from checkpoint replay.
6. The evidence controller reads the paused run's exact `owner_thread_id` from
   its private durable metadata, then restarts the genuine TUI as
   `just codex resume <owner_thread_id>` with the same disposable Codex home,
   project, provider config, and log directory. The visual-only driver must not
   inspect the metadata or choose a different thread. In that exact resumed
   owner thread, select the paused run and resume it without re-entering or
   displaying its arguments.
7. Capture a fresh run ID, an explicit `resumed from <oldRunId>` relationship,
   progress from the exact durable script, and normal terminal completion.
8. The evidence verifier checks canonical private `invocation.json`, exact
   script/args/fingerprint reuse, the same authorized owner, a fresh successor
   run ID with separate resume lineage, and no args or secret-derived fragments
   in notifications, logs, traces, or cards.

The paired non-null-argument lane is the real app-server integration test
`pause_and_resume_use_quiescent_private_immutable_checkpoint`, invoked with
`just test -p codex-app-server pause_and_resume_use_quiescent_private_immutable_checkpoint`.
It passes a private non-null argument, removes the mutable registry source,
proves same-owner authorization and idempotence, and asserts the resumed child
received the original value without exposing it on the API.

The judge fails the run for pause masquerading as stop/failure, response before
joined cleanup, implicit/latest-run targeting, resuming a non-paused or
ownerless run, re-reading a mutable registry source, argument re-entry, or
changing the ordinary parent workflow relationship.

## Selected-agent skip/retry-UAT

Use a two-agent disposable workflow so the driver can prove that controls apply
only to the explicitly selected live attempt and do not cancel the sibling or
restart the whole run. The mock router holds each attempt in `sleep 120` on
POSIX or `Start-Sleep -Seconds 120` on Windows.

1. Drill into monitor selection and highlight one live agent. Only that row may
   advertise `x skip` and `r retry`; the full run continues to advertise its
   distinct whole-run controls.
2. Press `x`, observe a destructive confirmation with **Cancel** selected, and
   cancel once. Reopen, explicitly confirm, and prove the logical `agent()`
   resolves `null` only after joined child/worktree/permit cleanup.
3. Prove the sibling continues and the parent workflow does not restart.
4. In a fresh run, select one live attempt and press `r`. Cancel once, then
   explicitly confirm. Capture a fresh child ID on the same logical node, an
   incremented bounded attempt number/reason, and no settlement of the outer
   `agent()` promise between attempts.
5. Prove failed-attempt tokens/tools/duration remain accumulated, scheduler and
   budget lifetime are charged again, the sibling remains unaffected, and
   completion follows the retried attempt.
6. Deterministic tests (not six costly visual repetitions) prove the exact cap:
   five user retries after the initial attempt, with the sixth retry request
   rejected and the original call failed without creating a seventh child.

The judge fails the run for latest-agent inference, control of a completed row,
whole-run restart, ordinal reassignment, stale-response application, duplicate
control spam, outer-promise settlement during retry, lost failed-attempt spend,
or cleanup acknowledgement while any attempt resource is still live.

## Strict headless semantic lanes

Run each canonical fixture in a fresh standalone project, Codex home, SQLite
home, and loopback mock process. Use the source-built `codex workflow run` and
the same process-host binary frozen for the TUI pass. The complete transcript
must match the manifest's exact route order and request count:

| Lane | Workflow | Required proof | Requests |
| --- | --- | --- | ---: |
| `failure/null` | `uat-failure-null` | natural `null`, sibling survival, schema/model options | 3 |
| `budget-36` | `uat-budget --args '{"budget":{"total":36}}'` | exact getter snapshots and no post-ceiling spawn | 2 |
| `parallel barrier` | `uat-parallel-barrier` | fast result retained while the phase waits for held sibling | 3 |
| `pipeline stagger` | `uat-pipeline-stagger` | fast item reaches stage two before held item leaves stage one | 4 |
| `nesting` | `uat-nested-parent` | depth one succeeds and depth two rejects before any grandchild request | 0 |
| `worktree` | `uat-worktree` | isolated cwd, marker create/read/remove, clean Git state, no namespace debris | 2 |
| `stop` | `uat-stop` | one child tool request and no natural final after explicit Stop | 1 |
| `skip` | `uat-agent-control` | selected child ends `null`; sibling reaches its natural final | 3 |
| `retry` | `uat-agent-control` | selected node gets a fresh attempt; sibling remains single-shot | 5 |
| `pause/resume` | `uat-pause` | first attempt is interrupted and immutable successor completes | 3 |

Controller assertions must also inspect the user-visible/CLI result, durable
journal and projection, private modes, fixture Git state, and absence of
unexpected markers. Transcript shape alone is necessary but not sufficient.

## Evidence verifier

For every lane, the non-visual evidence controller records repository, branch,
HEAD and dirty-tree identity; Codex and host hashes; mock, rendered-config, and
canonical fixture hashes; exact geometry; mock transcript; ordered frame and
keystroke timestamps; exit status; and a sanitized trace excerpt. It also
verifies, as applicable:

- run directories are `0700` and private files are `0600` under `umask 0002`;
- control acknowledgement follows child, worktree, permit, recorder, and lease
  cleanup, with no duplicate control RPC;
- `meta.json` and `progress.json` agree on `stopped` or `paused`;
- Save writes exact `script.js` bytes only and never writes args, results,
  journals, transcripts, or `.claude` files;
- resume uses exact persisted script, canonical args, fingerprint, owner, and a
  fresh run carrying only `resumed_from_run_id` lineage; and
- retry preserves the logical node, increments the attempt ordinal, creates a
  fresh child, and accumulates prior-attempt tokens/tools/duration.

## Claude comparison

Repeat every equivalent disposable control scenario against the pinned installed
Claude build with a separate driver and judge. Record version, resolved
executable SHA-256, canonical fixture SHA-256, exact keys and frames, and every
inconclusive observation. When Claude lacks a Codex checkpoint or whole-run
control, record that as an explicit behavioral difference; never silently map
Claude's selected-attempt restart onto a whole-workflow restart or convert a
missing observation into parity.
