# 2026-07-19 workflow-control UAT diagnostics (not signoff)

These runs were performed while rescuing the interrupted Dynamic Workflows
implementation. They are retained because the blind drivers exposed real
harness, product-wording, and storage-permission defects. They are **not** the
fresh final-hash Control-UAT required for parity signoff.

## System under test

- Repository: current repository root (`./`)
- Source-built Codex SHA-256:
  `547bbc0f9b4059c9aa5f6b0192303025baba3284d5d5b32a9d52c92c1275945f`
- Source-built code-mode host SHA-256:
  `6d055c0df21a8bf56985bdc30419e2ff7baeeef477622210e2f966fd99f74343`
- Mock server SHA-256:
  `7b4ea54082d587b6c0ef92f0021a10a28f1f114fd12930931c9d43101e9259be`
- Canonical Codex fixture tar SHA-256 for this pass:
  `13ff5a81f564eabf782bf1ab000a4d1217ae7c74372d575d4b8960da181f1631`
- Geometry: `120x36`
- Launch: genuine `just codex`, `RUST_LOG=trace`, disposable `HOME`,
  `CODEX_HOME`, project, and `log_dir`, with the source-built
  `CODEX_CODE_MODE_HOST_PATH`.
- Provider: loopback-only deterministic Responses fixture; no auth and no paid
  request.

## Attempt 1: sandbox fixture failure

A no-context driver used only tmux frames and keystrokes, correctly opened
`/workflow`, completed `uat-zero`, and selected `uat-stop`. The run completed in
348 ms instead of holding. The verifier found the exact tool output:

```text
Exit code: 1
Wall time: 0 seconds
Output:
bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
```

This was a nested-host Bubblewrap limitation, not a Stop result. The driver
correctly returned FAIL/INCONCLUSIVE and exited without inspecting hidden state.
The disposable deterministic config was changed to bypass the platform sandbox
only for this loopback fixture; it must never be reused with a live provider.

## Attempt 2: invalid driver procedure

The next fresh driver typed `uat-zero` and `uat-stop` as normal chat prompts
instead of opening the `/workflow` picker. That exercised ordinary model/tool
chat, not Dynamic Workflows. The bounded `sleep 45` did run, confirming the
fixture-config repair, but this attempt is invalid and carries no product
verdict.

## Attempt 3: valid picker path, timing and wording findings

The fresh driver used `/workflow` and the visible picker. Its first `uat-stop`
run completed naturally after 46 seconds while the driver was still traversing
the monitor, so the aggregate attempt is FAIL. A second picker-started run
captured the entire real Stop control path before the timeout:

1. `uat-zero` remained a retained completed workflow card.
2. `uat-stop` rendered `running`, with an active `stop-target`, one tool, and a
   distinct full run ID.
3. Agent focus advertised `x skip · r retry`; whole-run Stop was absent.
4. Full-run focus advertised `x stop · p pause`.
5. The `Stop workflow?` dialog selected `Cancel` by default. Enter canceled and
   left the exact run active.
6. Reopening, moving explicitly to `Stop selected workflow`, and confirming
   produced:

```text
Stopped workflow `uat-stop`. Run 019f7a23-10d7-7af1-8a0c-050e5f0ffaa4

Workflow uat-stop  …5f0ffaa4  stopped · duration 18s
stop  cancellation applied
0 tokens · 1 tool
Hold · 1/1 agents · duration 18s
stop-target  fixture-model · medium  interrupted · attempt 1 · null · 1 tool
```

7. `/exit` was visibly selected and the tmux session ended cleanly.

The driver found that the full-run footer said `x stop`, while the UAT rubric
requires the unambiguous `x stop workflow`. The TUI label and snapshots were
updated. The fixture hold was increased from 45 to 120 seconds so frame-by-frame
no-context drivers can finish without changing the control behavior under test.

## Artifact verifier finding

The exact stopped run had `meta.json.status == "stopped"` and terminal
`progress.json.status == "stopped"`; its child was `interrupted` and returned
null. However, under the host's `umask 0002`, the pre-fix run layout showed:

```text
0664 script.js
0664 journal.jsonl
0664 lease.lock
0600 invocation.json
0600 meta.json
0600 progress.json
```

The run directories were also group-accessible. This independently confirmed
the final review's privacy finding. Final-hash UAT must verify private directory
and file permissions after the journal hardening lands.

## Required rerun

No result above may be promoted to PASS. Rebuild after the privacy/durability,
wire-compatibility, recovery-fairness, and footer fixes; record new executable,
host, mock, config, and fixture hashes; then rerun Stop, Save, Pause/Resume,
Skip, and Retry with fresh no-context drivers and separate judges.
