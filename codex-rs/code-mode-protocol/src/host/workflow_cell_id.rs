use std::fmt;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;

use super::WireCellId;

const WORKFLOW_CELL_ID_PREFIX: &str = "wf:1:";
const WORKFLOW_CELL_EPOCH_HEX_BYTES: usize = 32;
const WORKFLOW_CELL_SEQUENCE_OFFSET: usize =
    WORKFLOW_CELL_ID_PREFIX.len() + WORKFLOW_CELL_EPOCH_HEX_BYTES + 1;

/// A client-assigned Saved-workflow cell identity for protocol V2.
///
/// While the identity capability is selected, one connection uses one fresh
/// epoch and emits strictly increasing sequences in request-frame order.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WireWorkflowCellId(WireCellId);

/// Failure returned for a noncanonical Saved-workflow cell identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidWireWorkflowCellId;

impl fmt::Display for InvalidWireWorkflowCellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid code-mode workflow cell ID")
    }
}

impl std::error::Error for InvalidWireWorkflowCellId {}

impl WireWorkflowCellId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, InvalidWireWorkflowCellId> {
        let cell_id = WireCellId::try_new(value).map_err(|_| InvalidWireWorkflowCellId)?;
        Self::try_from(cell_id)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns the lowercase hexadecimal epoch carried by this identity.
    pub fn epoch(&self) -> &str {
        &self.as_str()[WORKFLOW_CELL_ID_PREFIX.len()..WORKFLOW_CELL_SEQUENCE_OFFSET - 1]
    }

    /// Returns the nonzero sequence carried by this identity.
    pub fn sequence(&self) -> u64 {
        self.as_str()[WORKFLOW_CELL_SEQUENCE_OFFSET..]
            .bytes()
            .fold(0, |sequence, digit| sequence * 10 + u64::from(digit - b'0'))
    }
}

impl fmt::Display for WireWorkflowCellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for WireWorkflowCellId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireWorkflowCellId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl From<WireWorkflowCellId> for WireCellId {
    fn from(value: WireWorkflowCellId) -> Self {
        value.0
    }
}

impl From<&WireWorkflowCellId> for WireCellId {
    fn from(value: &WireWorkflowCellId) -> Self {
        value.0.clone()
    }
}

impl TryFrom<WireCellId> for WireWorkflowCellId {
    type Error = InvalidWireWorkflowCellId;

    fn try_from(value: WireCellId) -> Result<Self, Self::Error> {
        ensure_valid_workflow_cell_id(value.as_str())?;
        Ok(Self(value))
    }
}

fn ensure_valid_workflow_cell_id(value: &str) -> Result<(), InvalidWireWorkflowCellId> {
    let Some(suffix) = value.strip_prefix(WORKFLOW_CELL_ID_PREFIX) else {
        return Err(InvalidWireWorkflowCellId);
    };
    let Some((epoch, sequence)) = suffix.split_once(':') else {
        return Err(InvalidWireWorkflowCellId);
    };
    if epoch.len() != WORKFLOW_CELL_EPOCH_HEX_BYTES
        || !epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InvalidWireWorkflowCellId);
    }
    let sequence = sequence
        .parse::<u64>()
        .map_err(|_| InvalidWireWorkflowCellId)?;
    if sequence == 0 || sequence.to_string() != suffix[(epoch.len() + 1)..] {
        return Err(InvalidWireWorkflowCellId);
    }
    Ok(())
}

#[cfg(test)]
#[path = "workflow_cell_id_tests.rs"]
mod tests;
