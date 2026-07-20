use super::*;

impl StateRuntime {
    /// Decide whether recovery may use only the indexed running-ID path.
    ///
    /// The first caller after a fresh or recovered database atomically claims a
    /// cursor reset. Later startups continue that cursor until recovery marks a
    /// complete lexicographic filesystem cycle.
    pub async fn prepare_workflow_run_filesystem_index(
        &self,
    ) -> anyhow::Result<WorkflowRunFilesystemIndexState> {
        let mut transaction = self.pool.begin().await?;
        let (complete, started) = sqlx::query_as::<_, (bool, bool)>(
            r#"
SELECT filesystem_index_complete, backfill_started
FROM workflow_recovery_state
WHERE singleton = 1
            "#,
        )
        .fetch_one(&mut *transaction)
        .await?;
        let state = if complete {
            WorkflowRunFilesystemIndexState::Complete
        } else if started {
            WorkflowRunFilesystemIndexState::ContinueCursor
        } else {
            sqlx::query(
                r#"
UPDATE workflow_recovery_state
SET backfill_started = 1
WHERE singleton = 1
                "#,
            )
            .execute(&mut *transaction)
            .await?;
            WorkflowRunFilesystemIndexState::ResetCursor
        };
        transaction.commit().await?;
        Ok(state)
    }

    /// Restart filesystem indexing from the deterministic origin.
    pub async fn reset_workflow_run_filesystem_index(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"
UPDATE workflow_recovery_state
SET filesystem_index_complete = 0,
    backfill_started = 1,
    backfill_unresolved = 0
WHERE singleton = 1
            "#,
        )
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Remember that at least one canonical directory in the current cursor
    /// cycle could not be projected safely.
    pub async fn note_workflow_run_filesystem_index_unresolved(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"
UPDATE workflow_recovery_state
SET backfill_unresolved = 1
WHERE singleton = 1
            "#,
        )
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Finish a full cursor cycle, restarting when any earlier page was
    /// unresolved and otherwise committing the projection as complete.
    pub async fn finish_workflow_run_filesystem_index_cycle(
        &self,
    ) -> anyhow::Result<WorkflowRunFilesystemCycleResult> {
        let mut transaction = self.pool.begin().await?;
        let unresolved = sqlx::query_scalar::<_, bool>(
            r#"
SELECT backfill_unresolved
FROM workflow_recovery_state
WHERE singleton = 1
            "#,
        )
        .fetch_one(&mut *transaction)
        .await?;
        let result = if unresolved {
            sqlx::query(
                r#"
UPDATE workflow_recovery_state
SET filesystem_index_complete = 0,
    backfill_started = 1,
    backfill_unresolved = 0
WHERE singleton = 1
                "#,
            )
            .execute(&mut *transaction)
            .await?;
            WorkflowRunFilesystemCycleResult::Restart
        } else {
            sqlx::query(
                r#"
UPDATE workflow_recovery_state
SET filesystem_index_complete = 1,
    backfill_started = 1,
    backfill_unresolved = 0
WHERE singleton = 1
                "#,
            )
            .execute(&mut *transaction)
            .await?;
            WorkflowRunFilesystemCycleResult::Complete
        };
        transaction.commit().await?;
        Ok(result)
    }

    /// Make the filesystem index incomplete, then delete a bounded set of
    /// uncommitted publication reservations in the same SQLite transaction.
    ///
    /// The caller holds the cross-process publication/recovery lock, so none of
    /// these rows can belong to a live publisher. Any partial artifacts remain
    /// available to the filesystem backfill for authenticated classification.
    /// The caller durably resets the cursor only after this transaction commits;
    /// a crash at that cross-store boundary therefore leaves the DB incomplete.
    pub async fn discard_pending_workflow_run_publications(
        &self,
        limit: usize,
    ) -> anyhow::Result<WorkflowRunPendingCleanupBatch> {
        if limit == 0 {
            anyhow::bail!("workflow pending-publication cleanup limit must be positive");
        }
        let query_limit = i64::try_from(limit)?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("workflow cleanup batch limit is too large"))?;
        let mut transaction = self.pool.begin().await?;
        let mut run_ids = sqlx::query_scalar::<_, String>(
            r#"
SELECT run_id
FROM workflow_runs
WHERE publication_state = 'pending'
ORDER BY run_id ASC
LIMIT ?
            "#,
        )
        .bind(query_limit)
        .fetch_all(&mut *transaction)
        .await?;
        let has_more = run_ids.len() > limit;
        run_ids.truncate(limit);
        if !run_ids.is_empty() {
            sqlx::query(
                r#"
UPDATE workflow_recovery_state
SET filesystem_index_complete = 0,
    backfill_started = 1,
    backfill_unresolved = 0
WHERE singleton = 1
                "#,
            )
            .execute(&mut *transaction)
            .await?;
            let mut builder = QueryBuilder::<Sqlite>::new(
                "DELETE FROM workflow_runs WHERE publication_state = 'pending' AND run_id IN (",
            );
            let mut separated = builder.separated(", ");
            for run_id in &run_ids {
                separated.push_bind(run_id);
            }
            separated.push_unseparated(")");
            builder.build().execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(WorkflowRunPendingCleanupBatch {
            removed_run_ids: run_ids,
            has_more,
        })
    }

    /// Claim a bounded recovery batch of running workflow IDs.
    ///
    /// Selection is deterministic and durably rotates rows that survive an
    /// inspection, so held or damaged runs cannot permanently starve later
    /// rows across process starts. The returned IDs are ordered first by the
    /// number of prior recovery attempts and then lexicographically.
    pub async fn claim_running_workflow_runs_for_recovery(
        &self,
        limit: usize,
    ) -> anyhow::Result<WorkflowRunRecoveryBatch> {
        let query_limit = i64::try_from(limit)?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("workflow recovery batch limit is too large"))?;
        if limit == 0 {
            anyhow::bail!("workflow recovery batch limit must be positive");
        }

        let mut transaction = self.pool.begin().await?;
        let mut run_ids = sqlx::query_scalar::<_, String>(
            r#"
SELECT run_id
FROM workflow_runs
WHERE status = 'running' AND publication_state = 'committed'
ORDER BY recovery_attempts ASC, run_id ASC
LIMIT ?
            "#,
        )
        .bind(query_limit)
        .fetch_all(&mut *transaction)
        .await?;
        let has_more = run_ids.len() > limit;
        run_ids.truncate(limit);

        if !run_ids.is_empty() {
            let mut builder = QueryBuilder::<Sqlite>::new(
                "UPDATE workflow_runs SET recovery_attempts = recovery_attempts + 1 WHERE run_id IN (",
            );
            let mut separated = builder.separated(", ");
            for run_id in &run_ids {
                separated.push_bind(run_id);
            }
            separated.push_unseparated(")");
            builder.build().execute(&mut *transaction).await?;
        }
        transaction.commit().await?;

        Ok(WorkflowRunRecoveryBatch { run_ids, has_more })
    }
}

#[cfg(test)]
#[path = "workflow_run_recovery_tests.rs"]
mod tests;
