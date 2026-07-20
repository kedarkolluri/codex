-- Durable root-thread ownership for workflow mutation authorization.
-- Legacy rows remain ownerless and are still readable/rebuildable.
ALTER TABLE workflow_runs ADD COLUMN owner_thread_id TEXT;
