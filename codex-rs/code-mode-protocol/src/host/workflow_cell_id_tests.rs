use pretty_assertions::assert_eq;
use serde_json::json;

use super::InvalidWireWorkflowCellId;
use super::WireWorkflowCellId;
use crate::host::WireCellId;

#[test]
fn workflow_cell_id_accepts_only_canonical_components() {
    for (value, epoch, sequence) in [
        (
            "wf:1:00000000000000000000000000000000:1",
            "00000000000000000000000000000000",
            1,
        ),
        (
            "wf:1:0123456789abcdef0123456789abcdef:42",
            "0123456789abcdef0123456789abcdef",
            42,
        ),
        (
            "wf:1:ffffffffffffffffffffffffffffffff:18446744073709551615",
            "ffffffffffffffffffffffffffffffff",
            u64::MAX,
        ),
    ] {
        let identity = WireWorkflowCellId::try_new(value).expect("workflow cell identity");
        assert_eq!((identity.epoch(), identity.sequence()), (epoch, sequence));
        assert_eq!(identity.as_str(), value);
        assert_eq!(identity.to_string(), value);
        assert_eq!(
            serde_json::to_value(&identity).expect("encode workflow cell identity"),
            json!(value)
        );
        assert_eq!(
            serde_json::from_value::<WireWorkflowCellId>(json!(value))
                .expect("decode workflow cell identity"),
            identity
        );
        let wire_cell_id = WireCellId::from(&identity);
        assert_eq!(
            WireWorkflowCellId::try_from(wire_cell_id.clone())
                .expect("workflow cell ID from wire ID"),
            identity
        );
        assert_eq!(WireCellId::from(identity), wire_cell_id);
    }
}

#[test]
fn workflow_cell_id_rejects_noncanonical_components() {
    for value in [
        "",
        "cell-1",
        "wf:2:00000000000000000000000000000000:1",
        "wf:1:0000000000000000000000000000000:1",
        "wf:1:000000000000000000000000000000000:1",
        "wf:1:0000000000000000000000000000000g:1",
        "wf:1:0000000000000000000000000000000A:1",
        "wf:1:00000000000000000000000000000000:",
        "wf:1:00000000000000000000000000000000:0",
        "wf:1:00000000000000000000000000000000:01",
        "wf:1:00000000000000000000000000000000:+1",
        "wf:1:00000000000000000000000000000000:1:2",
        "wf:1:00000000000000000000000000000000:18446744073709551616",
        "wf:1:00000000000000000000000000000000:\n1",
    ] {
        assert_eq!(
            WireWorkflowCellId::try_new(value),
            Err(InvalidWireWorkflowCellId)
        );
        assert!(serde_json::from_value::<WireWorkflowCellId>(json!(value)).is_err());
    }
}
