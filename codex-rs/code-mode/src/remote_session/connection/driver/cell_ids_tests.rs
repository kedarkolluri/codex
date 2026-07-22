use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WIRE_CELL_ID_MAX_BYTES;
use codex_code_mode_protocol::host::WireCellId;
use pretty_assertions::assert_eq;

use super::RemoteSession;
use super::public_cell_id;
use super::remote_cell_id;

#[test]
fn max_length_wire_cell_id_round_trips_through_generation_prefix() {
    let wire_id = WireCellId::try_new("x".repeat(WIRE_CELL_ID_MAX_BYTES))
        .expect("maximum-length wire cell ID");
    let public_id = public_cell_id(u64::MAX, &wire_id);
    let session = RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation: u64::MAX,
    };

    assert_eq!(remote_cell_id(&session, &public_id), Ok(wire_id));
}

#[test]
fn restarted_generation_rejects_invalid_remote_cell_id_suffixes() {
    let session = RemoteSession {
        id: SessionId::new("session").expect("session ID"),
        generation: u64::MAX,
    };
    let oversized = "x".repeat(WIRE_CELL_ID_MAX_BYTES + 1);

    for remote_id in ["", "invalid\ncell", oversized.as_str()] {
        let public_id = CellId::new(format!("g{}:{remote_id}", session.generation));
        assert_eq!(
            remote_cell_id(&session, &public_id),
            Err("invalid code-mode cell ID".to_string())
        );
    }
}
