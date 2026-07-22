use pretty_assertions::assert_eq;
use serde_json::json;

use super::InvalidWireCellId;
use super::WIRE_CELL_ID_MAX_BYTES;
use super::WireCellId;

#[test]
fn wire_cell_id_accepts_bounded_safe_identifiers() {
    let max_length_id = "a".repeat(WIRE_CELL_ID_MAX_BYTES);
    let max_length_multibyte_id = "é".repeat(WIRE_CELL_ID_MAX_BYTES / 2);
    for value in [
        "1",
        "cell-1",
        "cell_name.2",
        "cell id/with:legacy-punctuation",
        "café",
        max_length_id.as_str(),
        max_length_multibyte_id.as_str(),
    ] {
        let cell_id = WireCellId::try_new(value).expect("valid wire cell ID");
        assert_eq!(cell_id.as_str(), value);
        assert_eq!(
            serde_json::from_value::<WireCellId>(json!(value)).expect("decode wire cell ID"),
            cell_id
        );
    }
}

#[test]
fn wire_cell_id_rejects_control_bearing_or_oversized_identifiers() {
    let oversized = "a".repeat(WIRE_CELL_ID_MAX_BYTES + 1);
    let oversized_multibyte = "é".repeat(WIRE_CELL_ID_MAX_BYTES / 2 + 1);
    for value in [
        "",
        "cell\n1",
        "cell\0one",
        "cell\u{0085}one",
        &oversized,
        &oversized_multibyte,
    ] {
        assert_eq!(WireCellId::try_new(value), Err(InvalidWireCellId));
        assert!(serde_json::from_value::<WireCellId>(json!(value)).is_err());
        assert!(serde_json::to_value(WireCellId::new(value)).is_err());
    }
}
