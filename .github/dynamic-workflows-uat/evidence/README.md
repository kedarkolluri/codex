# Dynamic Workflows UAT evidence

The reports in this directory contain sanitized evidence from the 2026-07-18
through 2026-07-19 rescue. Raw PTY transcripts and trace logs remain in their
disposable `/tmp` roots because they can contain account or session data and are
not safe repository artifacts.

## Final-hash signoff evidence

- [Codex final-hash TUI and control UAT](2026-07-19-codex-final-tui.md)
- [Claude Code controls parity](2026-07-19-claude-controls-parity.md)

The final interactive lanes used fresh visual-only drivers restricted to tmux
output and keystrokes. Separate no-context judges received sanitized evidence
packets and rubrics; they did not inspect source, logs, files, tmux, processes,
network state, identity, or credentials. Codex deterministic lanes additionally
used the canonical fixture manifest and fail-closed transcript verifier.

## Historical and recovery evidence

- [Codex pre-control TUI baseline](2026-07-18-codex-tui.md) — historical
  earlier-hash PASS, not final control signoff
- [Claude Code 2.1.201 baseline](2026-07-18-claude-2.1.201.md) — historical
  baseline comparison, not final controls parity
- [Workflow-control diagnostics](2026-07-19-control-uat-diagnostics.md) —
  diagnostic attempts only; explicitly not signoff
- [Pre-control rescue checkpoint](2026-07-19-rescue-checkpoint.md) — immutable
  recovery evidence, not final delivery state

The sibling [Control UAT protocol](../CONTROL_UAT.md) is the governing rubric;
the protocol itself is not evidence of a pass.
