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

    /// Reserve a discovery row before any run artifact is published.
    ///
    /// The caller must hold the workflow publication/recovery lock until it
    /// either commits or aborts this reservation. Conflicts are accepted only
    /// when every immutable identity field matches, which lets concurrent
    /// resume callers converge without regressing an existing terminal status.
    pub async fn begin_workflow_run_publication(
        &self,
        params: &WorkflowRunUpsertParams,
    ) -> anyhow::Result<WorkflowRunPublicationAdmission> {
        if params.status != WorkflowRunStatus::Running {
            anyhow::bail!("workflow publication reservations must start running");
        }
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            r#"
INSERT INTO workflow_runs (
    run_id,
    name,
    script_hash,
    script_path,
    parent_run_id,
    resumed_from_run_id,
    owner_thread_id,
    status,
    created_at,
    publication_state
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending')
ON CONFLICT(run_id) DO NOTHING
            "#,
        )
        .bind(params.run_id.as_str())
        .bind(params.name.as_str())
        .bind(params.script_hash.as_str())
        .bind(params.script_path.as_str())
        .bind(params.parent_run_id.as_deref())
        .bind(params.resumed_from_run_id.as_deref())
        .bind(params.owner_thread_id.as_deref())
        .bind(params.status.as_str())
        .bind(params.created_at.as_str())
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        let admission = if inserted {
            WorkflowRunPublicationAdmission::InsertedPending
        } else {
            let existing_row = sqlx::query_as::<_, WorkflowRunRow>(
                r#"
SELECT run_id, name, script_hash, script_path, parent_run_id, resumed_from_run_id, owner_thread_id, status, created_at, publication_state
FROM workflow_runs
WHERE run_id = ?
                "#,
            )
            .bind(params.run_id.as_str())
            .fetch_one(&mut *transaction)
            .await?;
            let publication_state = existing_row.publication_state.clone();
            let existing = WorkflowRun::try_from(existing_row)?;
            ensure_same_workflow_run_identity(&existing, params)?;
            match publication_state.as_str() {
                "pending" => WorkflowRunPublicationAdmission::ExistingPending(existing.status),
                "committed" => WorkflowRunPublicationAdmission::ExistingCommitted(existing.status),
                state => anyhow::bail!(
                    "workflow run `{}` has invalid publication state `{state}`",
                    params.run_id
                ),
            }
        };
        transaction.commit().await?;
        Ok(admission)
    }

    /// Commit a pending row after all authenticated launch artifacts exist.
    pub async fn commit_workflow_run_publication(&self, run_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
UPDATE workflow_runs
SET publication_state = 'committed'
WHERE run_id = ? AND publication_state = 'pending'
            "#,
        )
        .bind(run_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Remove a pending row when artifact publication failed before commit.
    pub async fn abort_workflow_run_publication(&self, run_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "DELETE FROM workflow_runs WHERE run_id = ? AND publication_state = 'pending'",
        )
        .bind(run_id)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Transition a run's status (for example, `running -> completed|stopped|failed`) on finish.
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
  AND publication_state = 'committed'
  AND (status = 'running' OR status = ?)
            "#,
        )
        .bind(status.as_str())
        .bind(run_id)
        .bind(status.as_str())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch a single run by id.
    pub async fn get_workflow_run(&self, run_id: &str) -> anyhow::Result<Option<WorkflowRun>> {
        let row = sqlx::query_as::<_, WorkflowRunRow>(
            r#"
SELECT run_id, name, script_hash, script_path, parent_run_id, resumed_from_run_id, owner_thread_id, status, created_at, publication_state
FROM workflow_runs
WHERE run_id = ? AND publication_state = 'committed'
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
SELECT run_id, name, script_hash, script_path, parent_run_id, resumed_from_run_id, owner_thread_id, status, created_at, publication_state
FROM workflow_runs
WHERE publication_state = 'committed' AND name =
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
SELECT run_id, name, script_hash, script_path, parent_run_id, resumed_from_run_id, owner_thread_id, status, created_at, publication_state
FROM workflow_runs
WHERE publication_state = 'committed'
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
    resumed_from_run_id,
    owner_thread_id,
    status,
    created_at,
    publication_state
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'committed')
ON CONFLICT(run_id) DO UPDATE SET
    name = excluded.name,
    script_hash = excluded.script_hash,
    script_path = excluded.script_path,
    parent_run_id = excluded.parent_run_id,
    resumed_from_run_id = excluded.resumed_from_run_id,
    owner_thread_id = excluded.owner_thread_id,
    status = excluded.status,
    created_at = excluded.created_at,
    publication_state = 'committed'
        "#,
    )
    .bind(params.run_id.as_str())
    .bind(params.name.as_str())
    .bind(params.script_hash.as_str())
    .bind(params.script_path.as_str())
    .bind(params.parent_run_id.as_deref())
    .bind(params.resumed_from_run_id.as_deref())
    .bind(params.owner_thread_id.as_deref())
    .bind(params.status.as_str())
    .bind(params.created_at.as_str())
    .execute(executor)
    .await?;
    Ok(())
}

fn ensure_same_workflow_run_identity(
    existing: &WorkflowRun,
    expected: &WorkflowRunUpsertParams,
) -> anyhow::Result<()> {
    if existing.run_id != expected.run_id
        || existing.name != expected.name
        || existing.script_hash != expected.script_hash
        || existing.script_path != expected.script_path
        || existing.parent_run_id != expected.parent_run_id
        || existing.resumed_from_run_id != expected.resumed_from_run_id
        || existing.owner_thread_id != expected.owner_thread_id
        || existing.created_at != expected.created_at
    {
        anyhow::bail!(
            "workflow run `{}` publication conflicts with an existing identity",
            expected.run_id
        );
    }
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
            resumed_from_run_id: None,
            owner_thread_id: None,
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
        let owner_thread_id = "01900000-0000-7000-8000-000000000001";
        let mut inserted = params(
            "run-1",
            "triage",
            "2026-07-17T00:00:00Z",
            WorkflowRunStatus::Running,
        );
        inserted.owner_thread_id = Some(owner_thread_id.to_string());
        runtime.upsert_workflow_run(&inserted).await?;

        let fetched = runtime
            .get_workflow_run("run-1")
            .await?
            .expect("run should exist");
        let expected = WorkflowRun {
            run_id: "run-1".to_string(),
            name: "triage".to_string(),
            script_hash: "blake3:run-1".to_string(),
            script_path: "/runs/run-1/script.js".to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: Some(owner_thread_id.to_string()),
            status: WorkflowRunStatus::Running,
            created_at: "2026-07-17T00:00:00Z".to_string(),
        };
        assert_eq!(fetched, expected);

        let by_name = runtime.list_workflow_runs_by_name("triage", None).await?;
        assert_eq!(by_name, vec![expected]);

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
        let completed = WorkflowRun {
            run_id: "run-1".to_string(),
            name: "triage".to_string(),
            script_hash: "blake3:run-1".to_string(),
            script_path: "/runs/run-1/script.js".to_string(),
            parent_run_id: None,
            resumed_from_run_id: None,
            owner_thread_id: None,
            status: WorkflowRunStatus::Completed,
            created_at: "2026-07-17T00:00:00Z".to_string(),
        };
        assert_eq!(fetched, completed);

        assert!(
            !runtime
                .set_workflow_run_status("run-1", WorkflowRunStatus::Stopped)
                .await?,
            "a later terminal status must not relabel the first terminal writer"
        );
        assert!(
            runtime
                .set_workflow_run_status("run-1", WorkflowRunStatus::Completed)
                .await?,
            "repeating the winning terminal status is idempotent"
        );
        assert_eq!(
            runtime
                .get_workflow_run("run-1")
                .await?
                .expect("run should remain discoverable"),
            completed
        );

        // Updating an unknown run affects no rows.
        assert!(
            !runtime
                .set_workflow_run_status("missing", WorkflowRunStatus::Failed)
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn stopped_status_round_trips_through_the_discovery_index() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        runtime
            .upsert_workflow_run(&params(
                "run-1",
                "triage",
                "2026-07-17T00:00:00Z",
                WorkflowRunStatus::Running,
            ))
            .await?;

        assert!(
            runtime
                .set_workflow_run_status("run-1", WorkflowRunStatus::Stopped)
                .await?
        );
        assert_eq!(
            runtime
                .get_workflow_run("run-1")
                .await?
                .expect("stopped run should exist"),
            WorkflowRun {
                run_id: "run-1".to_string(),
                name: "triage".to_string(),
                script_hash: "blake3:run-1".to_string(),
                script_path: "/runs/run-1/script.js".to_string(),
                parent_run_id: None,
                resumed_from_run_id: None,
                owner_thread_id: None,
                status: WorkflowRunStatus::Stopped,
                created_at: "2026-07-17T00:00:00Z".to_string(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn paused_status_and_resume_lineage_round_trip() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        let mut inserted = params(
            "run-resumed",
            "triage",
            "2026-07-17T00:00:00Z",
            WorkflowRunStatus::Running,
        );
        inserted.resumed_from_run_id = Some("run-paused".to_string());
        runtime.upsert_workflow_run(&inserted).await?;
        assert!(
            runtime
                .set_workflow_run_status("run-resumed", WorkflowRunStatus::Paused)
                .await?
        );

        assert_eq!(
            runtime
                .get_workflow_run("run-resumed")
                .await?
                .expect("paused run should exist"),
            WorkflowRun {
                run_id: "run-resumed".to_string(),
                name: "triage".to_string(),
                script_hash: "blake3:run-resumed".to_string(),
                script_path: "/runs/run-resumed/script.js".to_string(),
                parent_run_id: None,
                resumed_from_run_id: Some("run-paused".to_string()),
                owner_thread_id: None,
                status: WorkflowRunStatus::Paused,
                created_at: "2026-07-17T00:00:00Z".to_string(),
            }
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
        let mut live_run = params(
            "run-1",
            "triage",
            "2026-07-15T00:00:00Z",
            WorkflowRunStatus::Running,
        );
        live_run.owner_thread_id = Some("01900000-0000-7000-8000-000000000001".to_string());
        runtime.upsert_workflow_run(&live_run).await?;
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
        let mut rebuilt_run = params(
            "run-1",
            "triage",
            "2026-07-15T00:00:00Z",
            WorkflowRunStatus::Completed,
        );
        rebuilt_run.owner_thread_id = live_run.owner_thread_id;
        let source = vec![
            rebuilt_run,
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

    #[tokio::test]
    async fn pending_publication_is_hidden_until_commit() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        let pending = params(
            "run-pending",
            "triage",
            "2026-07-19T00:00:00Z",
            WorkflowRunStatus::Running,
        );

        assert_eq!(
            runtime.begin_workflow_run_publication(&pending).await?,
            WorkflowRunPublicationAdmission::InsertedPending
        );
        assert_eq!(runtime.get_workflow_run("run-pending").await?, None);
        assert!(runtime.list_workflow_runs(None).await?.is_empty());
        assert!(
            runtime
                .list_workflow_runs_by_name("triage", None)
                .await?
                .is_empty()
        );
        assert!(
            !runtime
                .set_workflow_run_status("run-pending", WorkflowRunStatus::Failed)
                .await?
        );
        assert_eq!(
            runtime.claim_running_workflow_runs_for_recovery(1).await?,
            WorkflowRunRecoveryBatch {
                run_ids: Vec::new(),
                has_more: false,
            }
        );
        assert!(
            runtime
                .commit_workflow_run_publication("run-pending")
                .await?
        );
        assert_eq!(
            runtime
                .get_workflow_run("run-pending")
                .await?
                .expect("committed row becomes discoverable")
                .status,
            WorkflowRunStatus::Running
        );
        assert_eq!(runtime.list_workflow_runs(None).await?.len(), 1);
        assert_eq!(
            runtime
                .list_workflow_runs_by_name("triage", None)
                .await?
                .len(),
            1
        );
        assert_eq!(
            runtime.claim_running_workflow_runs_for_recovery(1).await?,
            WorkflowRunRecoveryBatch {
                run_ids: vec!["run-pending".to_string()],
                has_more: false,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_publication_reservations_converge_without_status_regression()
    -> anyhow::Result<()> {
        let runtime = runtime().await?;
        let pending = params(
            "run-concurrent",
            "triage",
            "2026-07-19T00:00:00Z",
            WorkflowRunStatus::Running,
        );
        let (first, second) = tokio::join!(
            runtime.begin_workflow_run_publication(&pending),
            runtime.begin_workflow_run_publication(&pending)
        );
        let admissions = [first?, second?];
        assert_eq!(
            admissions
                .iter()
                .filter(|admission| {
                    **admission == WorkflowRunPublicationAdmission::InsertedPending
                })
                .count(),
            1
        );
        assert_eq!(
            admissions
                .iter()
                .filter(|admission| {
                    **admission
                        == WorkflowRunPublicationAdmission::ExistingPending(
                            WorkflowRunStatus::Running,
                        )
                })
                .count(),
            1
        );
        assert!(
            runtime
                .commit_workflow_run_publication("run-concurrent")
                .await?
        );
        runtime
            .set_workflow_run_status("run-concurrent", WorkflowRunStatus::Completed)
            .await?;
        assert_eq!(
            runtime.begin_workflow_run_publication(&pending).await?,
            WorkflowRunPublicationAdmission::ExistingCommitted(WorkflowRunStatus::Completed)
        );
        assert_eq!(
            runtime
                .get_workflow_run("run-concurrent")
                .await?
                .expect("concurrent run remains indexed")
                .status,
            WorkflowRunStatus::Completed
        );
        Ok(())
    }

    #[tokio::test]
    async fn publication_conflict_never_adopts_a_different_identity() -> anyhow::Result<()> {
        let runtime = runtime().await?;
        let pending = params(
            "run-collision",
            "triage",
            "2026-07-19T00:00:00Z",
            WorkflowRunStatus::Running,
        );
        runtime.begin_workflow_run_publication(&pending).await?;
        let mut conflicting = pending;
        conflicting.script_hash = "blake3:different".to_string();

        assert!(
            runtime
                .begin_workflow_run_publication(&conflicting)
                .await
                .is_err()
        );
        Ok(())
    }
}
