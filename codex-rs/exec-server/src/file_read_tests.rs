use anyhow::Result;
use bytes::Bytes;
use pretty_assertions::assert_eq;

use super::FileReadBlock;
use super::FileReadHandleManager;
use super::MAX_RETAINED_SNAPSHOT_BYTES;

#[tokio::test]
async fn immutable_snapshot_reads_are_random_access_and_budgeted_until_close() -> Result<()> {
    let manager = FileReadHandleManager::default();
    let reservation = manager.reserve_snapshot(MAX_RETAINED_SNAPSHOT_BYTES as u64)?;
    manager
        .open_snapshot(
            "snapshot".to_string(),
            Bytes::from_static(b"012345"),
            reservation,
        )
        .await?;

    assert_eq!(
        manager
            .read_block("snapshot", /*offset*/ 2, /*len*/ 3)
            .await?,
        FileReadBlock {
            bytes: b"234".to_vec(),
            eof: false,
        }
    );
    assert_eq!(
        manager
            .read_block("snapshot", /*offset*/ 5, /*len*/ 3)
            .await?,
        FileReadBlock {
            bytes: b"5".to_vec(),
            eof: true,
        }
    );
    let remaining = manager.reserve_snapshot((MAX_RETAINED_SNAPSHOT_BYTES - 6) as u64)?;
    assert!(manager.reserve_snapshot(/*max_bytes*/ 1).is_err());

    drop(remaining);
    manager.close("snapshot").await;
    let _reservation = manager.reserve_snapshot(MAX_RETAINED_SNAPSHOT_BYTES as u64)?;
    Ok(())
}

#[test]
fn abandoned_snapshot_reservation_releases_capacity() -> Result<()> {
    let manager = FileReadHandleManager::default();
    let reservation = manager.reserve_snapshot(MAX_RETAINED_SNAPSHOT_BYTES as u64)?;
    drop(reservation);

    let _replacement = manager.reserve_snapshot(MAX_RETAINED_SNAPSHOT_BYTES as u64)?;
    Ok(())
}
