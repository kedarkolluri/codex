use super::*;
use crate::model::WorkflowRunRow;

impl StateRuntime {
    /// Insert (or replace) a `workflow_runs` discovery row. Called on run start
    /// to register the run, and reused by [`rebuild_workflow_runs`] to
    /// reconstruct the projection from `meta.json`.
    ///
    /// [`rebuild_workflow_runs`]: StateRuntime::rebuild_workflow_runs
    pub async fn upsert_workflow_run(
        &self,
        params: &WorkflowRunUpsertParams,
    ) -> anyhow::Result<()> {
        upsert_workflow_run_on(self.pool.as_ref(), params).await
    }

    /// Transition a run's status (e.g. `running -> completed|failed`) on finish.
    ///
    /// Only mutates `status`; the immutable discovery fields recorded at start
    /// are left untouched. Returns whether a row was updated.
    pub async fn set_workflow_run_status(
        &self,
        run_id: &str,
        status: WorkflowRunStatus,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
UPDATE workflow_runs
SET status = ?
WHERE run_id = ?
            "#,
        )
        .bind(status.as_str())
        .bind(run_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch a single run by id.
    pub async fn get_workflow_run(&self, run_id: &str) -> anyhow::Result<Option<WorkflowRun>> {
        let row = sqlx::query_as::<_, WorkflowRunRow>(
            r#"
SELECT run_id, name, script_hash, script_path, parent_run_id, status, created_at
FROM workflow_runs
WHERE run_id = ?
            "#,
        )
        .bind(run_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(WorkflowRun::try_from).transpose()
    }

    /// List runs with a given workflow name, newest-first (the `codex workflow
    /// ls` / picker discovery-by-name path).
    pub async fn list_workflow_runs_by_name(
        &self,
        name: &str,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<WorkflowRun>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT run_id, name, script_hash, script_path, parent_run_id, status, created_at
FROM workflow_runs
WHERE name =
            "#,
        );
        builder.push_bind(name);
        builder.push(" ORDER BY created_at DESC, run_id DESC");
        if let Some(limit) = limit {
            builder.push(" LIMIT ");
            builder.push_bind(limit as i64);
        }
        let rows: Vec<WorkflowRunRow> = builder
            .build_query_as::<WorkflowRunRow>()
            .fetch_all(self.pool.as_ref())
            .await?;
        rows.into_iter().map(WorkflowRun::try_from).collect()
    }

    /// List all runs, newest-first (the `codex workflow ls` listing path).
    pub async fn list_workflow_runs(
        &self,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<WorkflowRun>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT run_id, name, script_hash, script_path, parent_run_id, status, created_at
FROM workflow_runs
ORDER BY created_at DESC, run_id DESC
            "#,
        );
        if let Some(limit) = limit {
            builder.push(" LIMIT ");
            builder.push_bind(limit as i64);
        }
        let rows: Vec<WorkflowRunRow> = builder
            .build_query_as::<WorkflowRunRow>()
            .fetch_all(self.pool.as_ref())
            .await?;
        rows.into_iter().map(WorkflowRun::try_from).collect()
    }

    /// Rebuild the entire projection from a set of run rows recovered from the
    /// on-disk `runs/<runId>/meta.json` files. Clears the table and re-inserts
    /// atomically so the projection is fully rebuildable (spec §7, R8).
    pub async fn rebuild_workflow_runs(
        &self,
        runs: &[WorkflowRunUpsertParams],
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM workflow_runs")
            .execute(&mut *tx)
            .await?;
        for params in runs {
            upsert_workflow_run_on(&mut *tx, params).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn upsert_workflow_run_on<'e, E>(
    executor: E,
    params: &WorkflowRunUpsertParams,
) -> anyhow::Result<()>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        r#"
INSERT INTO workflow_runs (
    run_id,
    name,
    script_hash,
    script_path,
    parent_run_id,
    status,
    created_at
) VALUES (?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(run_id) DO UPDATE SET
    name = excluded.name,
    script_hash = excluded.script_hash,
    script_path = excluded.script_path,
    parent_run_id = excluded.parent_run_id,
    status = excluded.status,
    created_at = excluded.created_at
        "#,
    )
    .bind(params.run_id.as_str())
    .bind(params.name.as_str())
    .bind(params.script_hash.as_str())
    .bind(params.script_path.as_str())
    .bind(params.parent_run_id.as_deref())
    .bind(params.status.as_str())
    .bind(params.created_at.as_str())
    .execute(executor)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::unique_temp_dir;
    use pretty_assertions::assert_eq;

    fn params(
        run_id: &str,
        name: &str,
        created_at: &str,
        status: WorkflowRunStatus,
    ) -> WorkflowRunUpsertParams {
        WorkflowRunUpsertParams {
            run_id: run_id.to_string(),
            name: name.to_string(),
            script_hash: format!("blake3:{run_id}"),
            script_path: format!("/runs/{run_id}/script.js"),
            parent_run_id: None,
            status,
            created_at: created_at.to_string(),
        }
    }

    async fn runtime() -> anyhow::Result<std::sync::Arc<StateRuntime>> {
        StateRuntime::init(unique_temp_dir(), "test-provider".to_string()).await
    }

    #[tokio::test]
    async fn insert_and_lookup_by_name() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        runtime
            .upsert_workflow_run(&params(
                "run-1",
                "triage",
                "2026-07-17T00:00:00Z",
                WorkflowRunStatus::Running,
            ))
            .await?;

        let fetched = runtime
            .get_workflow_run("run-1")
            .await?
            .expect("run should exist");
        assert_eq!(fetched.name, "triage");
        assert_eq!(fetched.status, WorkflowRunStatus::Running);
        assert_eq!(fetched.script_path, "/runs/run-1/script.js");
        assert_eq!(fetched.created_at, "2026-07-17T00:00:00Z");

        let by_name = runtime.list_workflow_runs_by_name("triage", None).await?;
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].run_id, "run-1");

        // A name with no runs returns empty.
        assert!(
            runtime
                .list_workflow_runs_by_name("nonexistent", None)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn completion_updates_status() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        runtime
            .upsert_workflow_run(&params(
                "run-1",
                "triage",
                "2026-07-17T00:00:00Z",
                WorkflowRunStatus::Running,
            ))
            .await?;

        let updated = runtime
            .set_workflow_run_status("run-1", WorkflowRunStatus::Completed)
            .await?;
        assert!(updated);

        let fetched = runtime
            .get_workflow_run("run-1")
            .await?
            .expect("run should exist");
        assert_eq!(fetched.status, WorkflowRunStatus::Completed);
        // Immutable discovery fields untouched by the status transition.
        assert_eq!(fetched.name, "triage");
        assert_eq!(fetched.created_at, "2026-07-17T00:00:00Z");

        // Updating an unknown run affects no rows.
        assert!(
            !runtime
                .set_workflow_run_status("missing", WorkflowRunStatus::Failed)
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn lists_runs_newest_first() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        runtime
            .upsert_workflow_run(&params(
                "run-old",
                "triage",
                "2026-07-15T00:00:00Z",
                WorkflowRunStatus::Completed,
            ))
            .await?;
        runtime
            .upsert_workflow_run(&params(
                "run-new",
                "triage",
                "2026-07-17T00:00:00Z",
                WorkflowRunStatus::Running,
            ))
            .await?;
        runtime
            .upsert_workflow_run(&params(
                "run-other",
                "review",
                "2026-07-16T00:00:00Z",
                WorkflowRunStatus::Failed,
            ))
            .await?;

        let all = runtime.list_workflow_runs(None).await?;
        let order: Vec<&str> = all.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(order, vec!["run-new", "run-other", "run-old"]);

        let triage = runtime.list_workflow_runs_by_name("triage", None).await?;
        let triage_order: Vec<&str> = triage.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(triage_order, vec!["run-new", "run-old"]);

        let limited = runtime.list_workflow_runs(Some(1)).await?;
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].run_id, "run-new");
        Ok(())
    }

    #[tokio::test]
    async fn projection_is_rebuildable() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        // Simulate the durable meta.json set: some runs still running, some
        // finished with a status transition applied live.
        runtime
            .upsert_workflow_run(&params(
                "run-1",
                "triage",
                "2026-07-15T00:00:00Z",
                WorkflowRunStatus::Running,
            ))
            .await?;
        runtime
            .set_workflow_run_status("run-1", WorkflowRunStatus::Completed)
            .await?;
        runtime
            .upsert_workflow_run(&params(
                "run-2",
                "review",
                "2026-07-16T00:00:00Z",
                WorkflowRunStatus::Failed,
            ))
            .await?;

        let before = runtime.list_workflow_runs(None).await?;

        // The rebuild source recovered from runs/<runId>/meta.json — same fields.
        let source = vec![
            params(
                "run-1",
                "triage",
                "2026-07-15T00:00:00Z",
                WorkflowRunStatus::Completed,
            ),
            params(
                "run-2",
                "review",
                "2026-07-16T00:00:00Z",
                WorkflowRunStatus::Failed,
            ),
        ];
        runtime.rebuild_workflow_runs(&source).await?;

        let after = runtime.list_workflow_runs(None).await?;
        assert_eq!(before, after, "rebuild must reproduce identical rows");
        Ok(())
    }
}
