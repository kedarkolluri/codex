use std::fmt;

use codex_code_mode_protocol::host::WireWorkflowCellId;

/// Constant-space replay protection for client-assigned workflow cell identities.
#[derive(Default)]
pub(super) struct WorkflowCellSequenceGuard {
    epoch: Option<String>,
    high_watermark: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkflowCellSequenceRejected;

impl WorkflowCellSequenceGuard {
    /// Consumes an identity before its execute request is dispatched.
    ///
    /// Requests arrive in wire order, so one epoch and a monotonic high-watermark
    /// reject replay without retaining a tombstone for every completed cell.
    pub(super) fn reserve(
        &mut self,
        identity: &WireWorkflowCellId,
    ) -> Result<(), WorkflowCellSequenceRejected> {
        let epoch = identity.epoch();
        let sequence = identity.sequence();
        match &self.epoch {
            None => {
                self.epoch = Some(epoch.to_string());
                self.high_watermark = sequence;
                Ok(())
            }
            Some(accepted_epoch) if accepted_epoch == epoch && sequence > self.high_watermark => {
                self.high_watermark = sequence;
                Ok(())
            }
            Some(_) => Err(WorkflowCellSequenceRejected),
        }
    }
}

impl fmt::Display for WorkflowCellSequenceRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("workflow cell identity was rejected")
    }
}

impl std::error::Error for WorkflowCellSequenceRejected {}

#[cfg(test)]
#[path = "workflow_cell_ids_tests.rs"]
mod tests;
