use super::*;
use crate::model::WorkflowRunAgentRow;

const MAX_RUN_AGENTS: usize = 10_000;

impl StateRuntime {
    /// Insert or refresh one run-to-agent projection row.
    ///
    /// The referenced `workflow_runs` row must already exist. The durable
    /// workflow journal remains the source of truth for this mapping.
    pub async fn upsert_workflow_run_agent(
        &self,
        params: &WorkflowRunAgentUpsertParams,
    ) -> anyhow::Result<()> {
        upsert_workflow_run_agent_on(self.pool.as_ref(), params).await
    }

    /// List a run's child transcripts in deterministic invocation order.
    pub async fn list_workflow_run_agents(
        &self,
        run_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WorkflowRunAgent>> {
        validate_limit(limit)?;
        let rows = sqlx::query_as::<_, WorkflowRunAgentRow>(
            r#"
SELECT run_id, ordinal, thread_id, rollout_path
FROM run_agents
WHERE run_id = ?
ORDER BY ordinal ASC
LIMIT ?
            "#,
        )
        .bind(run_id)
        .bind(i64::try_from(limit)?)
        .fetch_all(self.pool.as_ref())
        .await?;
        rows.into_iter().map(WorkflowRunAgent::try_from).collect()
    }

    /// Atomically replace one run's projection with journal-recovered rows.
    pub async fn replace_workflow_run_agents(
        &self,
        run_id: &str,
        agents: &[WorkflowRunAgentUpsertParams],
    ) -> anyhow::Result<()> {
        if agents.len() > MAX_RUN_AGENTS {
            anyhow::bail!("workflow run agent rebuild exceeds the {MAX_RUN_AGENTS}-agent cap");
        }
        if let Some(agent) = agents.iter().find(|agent| agent.run_id != run_id) {
            anyhow::bail!(
                "workflow agent run id `{}` does not match replacement target `{run_id}`",
                agent.run_id
            );
        }

        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM run_agents WHERE run_id = ?")
            .bind(run_id)
            .execute(&mut *transaction)
            .await?;
        for agent in agents {
            upsert_workflow_run_agent_on(&mut *transaction, agent).await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

fn validate_limit(limit: usize) -> anyhow::Result<()> {
    if !(1..=MAX_RUN_AGENTS).contains(&limit) {
        anyhow::bail!("workflow run agent limit must be from 1 to {MAX_RUN_AGENTS}");
    }
    Ok(())
}

async fn upsert_workflow_run_agent_on<'e, E>(
    executor: E,
    params: &WorkflowRunAgentUpsertParams,
) -> anyhow::Result<()>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    if !params.rollout_path.is_absolute() {
        anyhow::bail!(
            "workflow agent rollout path is not absolute: {}",
            params.rollout_path.display()
        );
    }
    let ordinal = i64::try_from(params.ordinal)
        .map_err(|_| anyhow::anyhow!("workflow agent ordinal exceeds SQLite INTEGER range"))?;
    sqlx::query(
        r#"
INSERT INTO run_agents (run_id, ordinal, thread_id, rollout_path)
VALUES (?, ?, ?, ?)
ON CONFLICT(run_id, ordinal) DO UPDATE SET
    thread_id = excluded.thread_id,
    rollout_path = excluded.rollout_path
        "#,
    )
    .bind(params.run_id.as_str())
    .bind(ordinal)
    .bind(params.thread_id.to_string())
    .bind(params.rollout_path.display().to_string())
    .execute(executor)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "workflow_run_agents_tests.rs"]
mod tests;
