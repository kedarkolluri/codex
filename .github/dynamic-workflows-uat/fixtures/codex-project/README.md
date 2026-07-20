# Dynamic Workflows human UAT fixture

This disposable project exists only to give the PTY driver a small, harmless
file to inspect. The workflow agents must not modify this file.

`uat-stop` asks one child to run only `sleep 120`, providing a harmless bounded
window for explicit full-run stop and cleanup testing. A successful stop must
prevent its `UAT_STOP_UNEXPECTED_NATURAL_COMPLETION` marker from appearing.

`uat-pause` uses the same harmless bounded hold to prove durable pause and exact
resume with a non-secret marker argument. `uat-agent-control` holds two siblings
so skip/retry targeting cannot be confused with a whole-run restart.
