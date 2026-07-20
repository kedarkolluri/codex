# Dynamic Workflows human UAT fixture

This disposable project exists only to give the PTY driver a small, harmless
file to inspect. The workflow agents must not modify this file.

`uat-stop` asks one child to run only `sleep 120`, providing a harmless bounded
window for explicit full-run stop testing. A successful stop must prevent its
`UAT_STOP_UNEXPECTED_NATURAL_COMPLETION` marker from appearing.

`uat-agent-control` holds two harmless siblings so the installed product's
selected-attempt skip/retry semantics can be observed without mistaking them for
a whole-run restart.
