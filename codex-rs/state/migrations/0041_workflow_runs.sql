-- Dynamic Workflows discovery index (M3, P3-workflow-runs-index).
--
-- PURELY a discovery-by-name projection for `codex workflow ls` / the run
-- picker. Replay NEVER reads this table: JSONL journals under
-- `runs/<runId>/journal.jsonl` are authoritative and this table is a
-- rebuildable projection of the `runs/<runId>/meta.json` set (spec §7, R8).
CREATE TABLE workflow_runs (
    run_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    script_hash TEXT NOT NULL,
    script_path TEXT NOT NULL,
    parent_run_id TEXT,
    status TEXT NOT NULL,
    -- Host-supplied ISO-8601 creation timestamp, stored verbatim as TEXT so a
    -- rebuild from meta.json reproduces byte-identical rows.
    created_at TEXT NOT NULL
);

CREATE INDEX idx_workflow_runs_name ON workflow_runs(name, created_at DESC);
CREATE INDEX idx_workflow_runs_created_at ON workflow_runs(created_at DESC);
