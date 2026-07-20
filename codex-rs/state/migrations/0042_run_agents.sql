-- Rebuildable run-to-agent transcript grouping for Dynamic Workflows.
--
-- `journal.jsonl` remains authoritative. This table only makes completed-run
-- monitoring and transcript drill-in cheap: callers can rebuild every row from
-- completed `agent_call` records carrying `child_thread_id` + `rollout_path`.
CREATE TABLE run_agents (
    run_id TEXT NOT NULL REFERENCES workflow_runs(run_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    thread_id TEXT NOT NULL,
    rollout_path TEXT NOT NULL,
    PRIMARY KEY (run_id, ordinal),
    UNIQUE (run_id, thread_id)
);

CREATE INDEX idx_run_agents_thread_id ON run_agents(thread_id);
