-- Explicit replay/checkpoint resume lineage, distinct from nested workflow parentage.
ALTER TABLE workflow_runs ADD COLUMN resumed_from_run_id TEXT;
