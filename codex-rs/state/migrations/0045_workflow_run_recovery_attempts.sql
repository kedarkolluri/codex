-- Durable rotation for bounded stale-workflow recovery. Rows that remain
-- running after inspection (for example, because another process holds their
-- lease) move behind less recently inspected rows on the next startup.
ALTER TABLE workflow_runs
ADD COLUMN recovery_attempts INTEGER NOT NULL DEFAULT 0;

-- A workflow start first inserts a pending row while holding the shared
-- publication/recovery lock, publishes its authenticated artifacts, and only
-- then commits the row. Existing rows predate that protocol and are already
-- committed by definition.
ALTER TABLE workflow_runs
ADD COLUMN publication_state TEXT NOT NULL DEFAULT 'committed'
    CHECK (publication_state IN ('pending', 'committed'));

CREATE INDEX idx_workflow_runs_recovery
ON workflow_runs(status, recovery_attempts, run_id);

CREATE INDEX idx_workflow_runs_publication_state
ON workflow_runs(publication_state, run_id);

-- A fresh/recovered SQLite projection starts incomplete. The bounded
-- filesystem cursor backfills it across startups and flips this singleton only
-- after completing one deterministic lexicographic cycle.
CREATE TABLE workflow_recovery_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    filesystem_index_complete INTEGER NOT NULL DEFAULT 0
        CHECK (filesystem_index_complete IN (0, 1)),
    backfill_started INTEGER NOT NULL DEFAULT 0
        CHECK (backfill_started IN (0, 1)),
    -- Persists an unresolved canonical directory across cursor pages. A full
    -- cycle with this bit set restarts from the origin instead of declaring the
    -- projection complete, so one corrupt entry cannot starve later entries.
    backfill_unresolved INTEGER NOT NULL DEFAULT 0
        CHECK (backfill_unresolved IN (0, 1))
);

INSERT INTO workflow_recovery_state (singleton) VALUES (1);
