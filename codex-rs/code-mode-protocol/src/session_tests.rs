use std::sync::Arc;

use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::BoundStartedCell;
use super::CellId;
use super::CodeModeSessionResultFuture;
use super::StartedCell;
use super::StartedCellBinding;
use super::WaitOutcome;

struct TestBinding {
    cell_id: CellId,
}

impl StartedCellBinding for TestBinding {
    fn cell_id(&self) -> &CellId {
        &self.cell_id
    }

    fn wait<'a>(&'a self, _yield_time_ms: u64) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { unreachable!("identity validation must not wait") })
    }

    fn terminate<'a>(&'a self) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { unreachable!("identity validation must not terminate") })
    }
}

#[tokio::test]
async fn started_cell_preserves_remote_initial_response_errors() {
    let (response_tx, response_rx) = oneshot::channel();
    response_tx
        .send(Err("remote runtime failed".to_string()))
        .expect("initial response receiver should be open");
    let started = StartedCell::from_result_receiver(CellId::new("1".to_string()), response_rx);

    assert_eq!(
        started.initial_response().await,
        Err("remote runtime failed".to_string())
    );
}

#[test]
fn bound_started_cell_rejects_a_mismatched_binding_identity() {
    let (_response_tx, response_rx) = oneshot::channel();
    let started_cell = StartedCell::new(CellId::new("started".to_string()), response_rx);
    let binding: Arc<dyn StartedCellBinding> = Arc::new(TestBinding {
        cell_id: CellId::new("bound".to_string()),
    });

    let Err(error) = BoundStartedCell::new(started_cell, binding) else {
        panic!("mismatched binding should be rejected");
    };

    assert_eq!(error, "started-cell binding identity mismatch");
}
