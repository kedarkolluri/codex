use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireWorkflowCellId;
use uuid::Uuid;

use super::cell_ids::public_cell_id as ordinary_public_cell_id;
use super::cell_ids::remote_cell_id as ordinary_remote_cell_id;
use super::types::RemoteSession;
use crate::remote_session::connection::handshake::NegotiatedWorkflowCellIdentity;

const WORKFLOW_CELL_ID_REJECTED: &str = "workflow cell identity was rejected";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CellProvenance {
    Ordinary,
    SavedWorkflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResolvedCellId {
    pub(super) wire_id: WireCellId,
    pub(super) provenance: CellProvenance,
}

pub(super) enum WorkflowCellNamespace {
    Unavailable,
    V1 { epoch: String },
}

impl WorkflowCellNamespace {
    pub(super) fn new(negotiated: NegotiatedWorkflowCellIdentity) -> Self {
        match negotiated {
            NegotiatedWorkflowCellIdentity::Unavailable => Self::Unavailable,
            NegotiatedWorkflowCellIdentity::V1 => Self::V1 {
                epoch: Uuid::new_v4().simple().to_string(),
            },
        }
    }

    pub(super) fn public_cell_id(&self, generation: u64, wire_id: &WireCellId) -> CellId {
        if self.provenance(wire_id) == CellProvenance::SavedWorkflow {
            CellId::new(wire_id.as_str().to_string())
        } else {
            ordinary_public_cell_id(generation, wire_id)
        }
    }

    pub(super) fn remote_cell_id(
        &self,
        session: &RemoteSession,
        public_id: &CellId,
    ) -> Result<ResolvedCellId, String> {
        if let Ok(wire_id) = WireCellId::try_new(public_id.as_str())
            && self.provenance(&wire_id) == CellProvenance::SavedWorkflow
        {
            return Ok(ResolvedCellId {
                wire_id,
                provenance: CellProvenance::SavedWorkflow,
            });
        }

        let wire_id = ordinary_remote_cell_id(session, public_id)?;
        if self.provenance(&wire_id) == CellProvenance::SavedWorkflow {
            return Err(WORKFLOW_CELL_ID_REJECTED.to_string());
        }
        Ok(ResolvedCellId {
            wire_id,
            provenance: CellProvenance::Ordinary,
        })
    }

    fn provenance(&self, wire_id: &WireCellId) -> CellProvenance {
        let Self::V1 { epoch } = self else {
            return CellProvenance::Ordinary;
        };
        match WireWorkflowCellId::try_from(wire_id.clone()) {
            Ok(workflow_cell_id) if workflow_cell_id.epoch() == epoch => {
                CellProvenance::SavedWorkflow
            }
            Ok(_) | Err(_) => CellProvenance::Ordinary,
        }
    }
}

#[cfg(test)]
#[path = "workflow_cell_ids_tests.rs"]
mod tests;
