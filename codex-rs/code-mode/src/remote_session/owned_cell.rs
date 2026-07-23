use std::sync::Arc;

use codex_code_mode_protocol::BoundStartedCell;
use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionResultFuture;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::StartedCellBinding;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;

use super::ProcessOwnedCodeModeSession;
use super::SessionBinding;

pub(super) async fn execute(
    session_owner: Arc<ProcessOwnedCodeModeSession>,
    session: SessionBinding,
    request: ExecuteRequest,
) -> Result<BoundStartedCell, String> {
    let started_cell = session
        .connection
        .execute(session.remote.clone(), request)
        .await?;
    let binding: Arc<dyn StartedCellBinding> = Arc::new(ProcessOwnedCellBinding {
        cell_id: started_cell.cell_id.clone(),
        _session_owner: session_owner,
        session,
    });
    BoundStartedCell::new(started_cell, binding)
}

struct ProcessOwnedCellBinding {
    cell_id: CellId,
    _session_owner: Arc<ProcessOwnedCodeModeSession>,
    session: SessionBinding,
}

impl StartedCellBinding for ProcessOwnedCellBinding {
    fn cell_id(&self) -> &CellId {
        &self.cell_id
    }

    fn wait<'a>(&'a self, yield_time_ms: u64) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(self.session.connection.wait(
            self.session.remote.clone(),
            WaitRequest {
                cell_id: self.cell_id.clone(),
                yield_time_ms,
            },
        ))
    }

    fn terminate<'a>(&'a self) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(
            self.session
                .connection
                .terminate(self.session.remote.clone(), self.cell_id.clone()),
        )
    }
}
