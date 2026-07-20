use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn register_test_turn(
        broker: &CodeModeDispatchBroker,
        owner_id: &str,
        routed_as: &str,
        records: Arc<Mutex<Vec<TestDispatchRecord>>>,
        schedulers: Arc<Mutex<Vec<WorkflowScheduler>>>,
    ) -> CodeModeDispatchWorker {
        let (response_tx, response_rx) = oneshot::channel();
        broker
            .command_tx
            .send(BrokerCommand::RegisterTurn {
                owner_id: owner_id.to_string(),
                context: DispatchContext::Test(Arc::new(TestTurnHostFactory {
                    owner_id: routed_as.to_string(),
                    records,
                    schedulers,
                })),
                response_tx,
            })
            .expect("dispatch broker is running");
        let generation = response_rx
            .await
            .expect("registration response")
            .expect("test turn registered");
        CodeModeDispatchWorker {
            command_tx: broker.command_tx.clone(),
            owner_id: owner_id.to_string(),
            generation,
        }
    }

    async fn bind_test_cell(
        broker: &CodeModeDispatchBroker,
        owner_id: &str,
        cell_id: &str,
    ) -> CellId {
        let cell_id = CellId::new(cell_id.to_string());
        broker
            .begin_cell_execution(CodeModeDispatchOrigin::Turn(owner_id.to_string()))
            .await
            .expect("begin cell execution")
            .bind(cell_id.clone())
            .await
            .expect("bind cell");
        broker.mark_cell_ready_for_dispatch(&cell_id);
        cell_id
    }

    fn cloned_records(records: &Arc<Mutex<Vec<TestDispatchRecord>>>) -> Vec<TestDispatchRecord> {
        match records.lock() {
            Ok(records) => records.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn enqueue_notify(
        broker: &CodeModeDispatchBroker,
        cell_id: CellId,
        ordinal: usize,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (response_tx, response_rx) = oneshot::channel();
        broker
            .command_tx
            .send(BrokerCommand::dispatch(DispatchMessage::Notify {
                call_id: format!("queued-{ordinal}"),
                cell_id,
                text: format!("message-{ordinal}"),
                cancellation_token: CancellationToken::new(),
                response_tx,
            }))
            .expect("dispatch broker is running");
        response_rx
    }

    async fn queued_error(receiver: oneshot::Receiver<Result<(), String>>) -> String {
        tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("queued dispatch resolves")
            .expect("broker returns a dispatch result")
            .expect_err("queued dispatch is rejected")
    }

    /// A callback for a cell that was never bound resolves promptly instead of creating an
    /// ownerless readiness gate and waiting forever.
    #[tokio::test]
    async fn spawn_agent_for_never_bound_cell_resolves_to_failed() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            broker.spawn_agent(
                CellId::new("never-bound".to_string()),
                /*node_id*/ 7,
                /*parent_node_id*/ None,
                /*phase*/ None,
                "spawn me".to_string(),
                /*ordinal*/ 7,
                AgentCallOpts::default(),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("unknown-cell dispatch resolved promptly");
        assert!(matches!(result, AgentSpawnOutcome::Failed));
    }

    /// A cancelled `agent()` call resolves to `Failed` (JS null) without ever throwing.
    #[tokio::test]
    async fn spawn_agent_resolves_to_failed_when_cancelled_before_dispatch() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let cancellation_token = CancellationToken::new();
        cancellation_token.cancel();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            broker.spawn_agent(
                CellId::new("cell-1".to_string()),
                /*node_id*/ 0,
                /*parent_node_id*/ None,
                /*phase*/ None,
                "prompt".to_string(),
                /*ordinal*/ 0,
                AgentCallOpts::default(),
                cancellation_token,
            ),
        )
        .await
        .expect("spawn_agent resolved promptly");
        assert!(
            matches!(result, AgentSpawnOutcome::Failed),
            "a cancelled call must resolve to Failed (null), never throw"
        );
    }

    #[tokio::test]
    async fn malformed_replay_record_is_rejected_instead_of_dropped() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));

        let error = broker
            .replay_agent(
                CellId::new("cell-1".to_string()),
                /*node_id*/ 0,
                /*parent_node_id*/ None,
                /*phase*/ None,
                serde_json::json!({"not": "an agent_call line"}),
            )
            .await
            .expect_err("malformed replay records must fail closed");

        assert_eq!(error, "code mode workflow replay record is invalid");
    }

    #[tokio::test]
    async fn overlapping_turn_cells_never_cross_route() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let schedulers = Arc::new(Mutex::new(Vec::new()));
        let _turn_a = register_test_turn(
            &broker,
            "turn-a",
            "host-a",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;
        let _turn_b = register_test_turn(
            &broker,
            "turn-b",
            "host-b",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;
        let cell_a = bind_test_cell(&broker, "turn-a", "cell-a").await;
        let cell_b = bind_test_cell(&broker, "turn-b", "cell-b").await;

        let (result_a, result_b) = tokio::join!(
            broker.notify(
                "call-a".to_string(),
                cell_a.clone(),
                "from-a".to_string(),
                CancellationToken::new(),
            ),
            broker.notify(
                "call-b".to_string(),
                cell_b.clone(),
                "from-b".to_string(),
                CancellationToken::new(),
            ),
        );
        assert_eq!((result_a, result_b), (Ok(()), Ok(())));
        let mut records = cloned_records(&records);
        records.sort_by(|left, right| left.owner_id.cmp(&right.owner_id));
        assert_eq!(
            records,
            vec![
                TestDispatchRecord {
                    owner_id: "host-a".to_string(),
                    cell_id: cell_a,
                    text: "from-a".to_string(),
                },
                TestDispatchRecord {
                    owner_id: "host-b".to_string(),
                    cell_id: cell_b,
                    text: "from-b".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn yielded_cell_keeps_original_host_after_turn_guard_drops() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let schedulers = Arc::new(Mutex::new(Vec::new()));
        let original = register_test_turn(
            &broker,
            "turn",
            "original-host",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;
        let cell = bind_test_cell(&broker, "turn", "yielded-cell").await;
        drop(original);
        let _replacement = register_test_turn(
            &broker,
            "turn",
            "replacement-host",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;

        let result = broker
            .notify(
                "late-call".to_string(),
                cell.clone(),
                "late".to_string(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result, Ok(()));
        assert_eq!(
            cloned_records(&records),
            vec![TestDispatchRecord {
                owner_id: "original-host".to_string(),
                cell_id: cell,
                text: "late".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn cells_under_one_turn_receive_distinct_schedulers() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let schedulers = Arc::new(Mutex::new(Vec::new()));
        let _turn =
            register_test_turn(&broker, "turn", "host", records, Arc::clone(&schedulers)).await;
        bind_test_cell(&broker, "turn", "cell-1").await;
        bind_test_cell(&broker, "turn", "cell-2").await;
        let schedulers = match schedulers.lock() {
            Ok(schedulers) => schedulers.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(schedulers.len(), 2);

        schedulers[0]
            .admit(|| async { SpawnAttempt::Finalized(()) })
            .await
            .expect("first cell admits independently");
        assert_eq!(
            (
                schedulers[0].lifetime_spawned(),
                schedulers[1].lifetime_spawned(),
            ),
            (1, 0)
        );
        schedulers[1]
            .admit(|| async { SpawnAttempt::Finalized(()) })
            .await
            .expect("second cell admits independently");
        assert_eq!(
            (
                schedulers[0].lifetime_spawned(),
                schedulers[1].lifetime_spawned(),
            ),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn closed_and_never_bound_callbacks_return_errors_promptly() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let schedulers = Arc::new(Mutex::new(Vec::new()));
        let _turn = register_test_turn(&broker, "turn", "host", records, schedulers).await;
        let closed_cell = bind_test_cell(&broker, "turn", "closed-cell").await;
        broker.close_cell(&closed_cell);

        let closed_error = tokio::time::timeout(
            Duration::from_secs(1),
            broker.notify(
                "closed".to_string(),
                closed_cell,
                "late".to_string(),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("closed-cell callback resolved")
        .expect_err("closed cell is rejected");
        assert!(closed_error.contains("closed"));

        let unknown_error = tokio::time::timeout(
            Duration::from_secs(1),
            broker.notify(
                "unknown".to_string(),
                CellId::new("never-bound".to_string()),
                "late".to_string(),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("never-bound callback resolved")
        .expect_err("never-bound cell is rejected");
        assert!(unknown_error.contains("not bound"));
    }

    #[tokio::test]
    async fn stale_turn_guard_drop_cannot_remove_replacement_registration() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let schedulers = Arc::new(Mutex::new(Vec::new()));
        let stale = register_test_turn(
            &broker,
            "turn",
            "stale-host",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;
        let _replacement = register_test_turn(
            &broker,
            "turn",
            "replacement-host",
            Arc::clone(&records),
            Arc::clone(&schedulers),
        )
        .await;
        drop(stale);
        tokio::task::yield_now().await;

        let cell = bind_test_cell(&broker, "turn", "replacement-cell").await;
        broker
            .notify(
                "call".to_string(),
                cell.clone(),
                "routed".to_string(),
                CancellationToken::new(),
            )
            .await
            .expect("replacement remains registered");
        assert_eq!(
            cloned_records(&records),
            vec![TestDispatchRecord {
                owner_id: "replacement-host".to_string(),
                cell_id: cell,
                text: "routed".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn pre_bind_queue_enforces_per_cell_limit_at_the_boundary() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let pending = broker
            .begin_cell_execution(CodeModeDispatchOrigin::Disabled)
            .await
            .expect("active execution permits pre-bind messages");
        let cell_id = CellId::new("pre-bind-per-cell".to_string());
        let mut accepted = (0..MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL)
            .map(|ordinal| enqueue_notify(&broker, cell_id.clone(), ordinal))
            .collect::<Vec<_>>();
        let overflow = enqueue_notify(&broker, cell_id, MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL);

        assert!(
            queued_error(overflow)
                .await
                .contains("pre-bind dispatch queue limit")
        );
        assert!(
            accepted.iter_mut().all(|receiver| matches!(
                receiver.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            )),
            "every message through the exact per-cell cap remains queued"
        );

        drop(pending);
        broker.finish_shutdown().await;
    }

    #[tokio::test]
    async fn pre_bind_queue_enforces_global_limit_at_the_boundary() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let pending = broker
            .begin_cell_execution(CodeModeDispatchOrigin::Disabled)
            .await
            .expect("active execution permits pre-bind messages");
        let mut accepted = Vec::with_capacity(MAX_UNBOUND_DISPATCH_MESSAGES);
        for ordinal in 0..MAX_UNBOUND_DISPATCH_MESSAGES {
            let cell = ordinal / MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL;
            accepted.push(enqueue_notify(
                &broker,
                CellId::new(format!("pre-bind-global-{cell}")),
                ordinal,
            ));
        }
        let overflow = enqueue_notify(
            &broker,
            CellId::new("pre-bind-global-overflow".to_string()),
            MAX_UNBOUND_DISPATCH_MESSAGES,
        );

        assert!(
            queued_error(overflow)
                .await
                .contains("pre-bind dispatch queue limit")
        );
        assert!(matches!(
            accepted.first_mut().expect("first queued").try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            accepted.last_mut().expect("last queued").try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        drop(pending);
        broker.finish_shutdown().await;
    }

    #[tokio::test]
    async fn pre_ready_queue_enforces_limit_at_the_boundary() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let cell_id = CellId::new("pre-ready".to_string());
        broker
            .begin_cell_execution(CodeModeDispatchOrigin::Disabled)
            .await
            .expect("begin disabled test cell")
            .bind(cell_id.clone())
            .await
            .expect("bind disabled test cell");
        let mut accepted = (0..MAX_PREPARED_CELL_MESSAGES)
            .map(|ordinal| enqueue_notify(&broker, cell_id.clone(), ordinal))
            .collect::<Vec<_>>();
        let overflow = enqueue_notify(&broker, cell_id.clone(), MAX_PREPARED_CELL_MESSAGES);

        assert!(
            queued_error(overflow)
                .await
                .contains("pre-ready dispatch queue limit")
        );
        assert!(matches!(
            accepted.first_mut().expect("first queued").try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            accepted.last_mut().expect("last queued").try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        broker.drain_workflow_cell(&cell_id).await;
        broker.finish_shutdown().await;
    }

    #[tokio::test]
    async fn closed_cell_tombstones_evict_only_after_the_cap() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        for ordinal in 0..=CLOSED_CELL_TOMBSTONE_CAP {
            broker.close_cell(&CellId::new(format!("closed-{ordinal}")));
        }
        // This command is a broker-ordering barrier after every close command above.
        let pending = broker
            .begin_cell_execution(CodeModeDispatchOrigin::Disabled)
            .await
            .expect("begin execution after tombstones are recorded");

        let newest = enqueue_notify(
            &broker,
            CellId::new(format!("closed-{CLOSED_CELL_TOMBSTONE_CAP}")),
            /*ordinal*/ 0,
        );
        assert!(queued_error(newest).await.contains("is closed"));

        let evicted = enqueue_notify(
            &broker,
            CellId::new("closed-0".to_string()),
            /*ordinal*/ 1,
        );
        drop(pending);
        assert!(queued_error(evicted).await.contains("was never bound"));

        broker.finish_shutdown().await;
    }
}
