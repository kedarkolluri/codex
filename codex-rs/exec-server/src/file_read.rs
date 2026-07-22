use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use codex_file_system::FILE_READ_CHUNK_SIZE;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const MAX_OPEN_FILE_READS: usize = 128;
const MAX_RETAINED_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct FileReadBlock {
    pub(crate) bytes: Vec<u8>,
    pub(crate) eof: bool,
}

struct ManagedFileReadHandle {
    backing: FileReadBacking,
    _slot: OwnedSemaphorePermit,
}

enum FileReadBacking {
    File(Arc<File>),
    Snapshot {
        bytes: Bytes,
        _byte_budget: OwnedSemaphorePermit,
    },
}

pub(crate) struct SnapshotReservation {
    byte_budget: OwnedSemaphorePermit,
    slot: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct FileReadHandleManager {
    handles: Arc<Mutex<HashMap<String, Arc<ManagedFileReadHandle>>>>,
    slots: Arc<Semaphore>,
    snapshot_bytes: Arc<Semaphore>,
}

impl Default for FileReadHandleManager {
    fn default() -> Self {
        Self {
            handles: Arc::new(Mutex::new(HashMap::new())),
            slots: Arc::new(Semaphore::new(MAX_OPEN_FILE_READS)),
            snapshot_bytes: Arc::new(Semaphore::new(MAX_RETAINED_SNAPSHOT_BYTES)),
        }
    }
}

impl FileReadHandleManager {
    pub(crate) async fn open(
        &self,
        handle_id: String,
        file: tokio::fs::File,
    ) -> io::Result<String> {
        let file = Arc::new(file.into_std().await);
        let slot = self.reserve_slot()?;
        self.insert(handle_id, FileReadBacking::File(file), slot)
            .await
    }

    pub(crate) fn reserve_snapshot(&self, max_bytes: u64) -> io::Result<SnapshotReservation> {
        let max_bytes = u32::try_from(max_bytes).map_err(|_| snapshot_budget_error())?;
        let slot = self.reserve_slot()?;
        let byte_budget = self
            .snapshot_bytes
            .clone()
            .try_acquire_many_owned(max_bytes)
            .map_err(|_| snapshot_budget_error())?;
        Ok(SnapshotReservation { byte_budget, slot })
    }

    pub(crate) async fn open_snapshot(
        &self,
        handle_id: String,
        bytes: Bytes,
        mut reservation: SnapshotReservation,
    ) -> io::Result<String> {
        let unused = reservation
            .byte_budget
            .num_permits()
            .checked_sub(bytes.len())
            .ok_or_else(snapshot_budget_error)?;
        if unused > 0 {
            drop(
                reservation
                    .byte_budget
                    .split(unused)
                    .ok_or_else(snapshot_budget_error)?,
            );
        }
        self.insert(
            handle_id,
            FileReadBacking::Snapshot {
                bytes,
                _byte_budget: reservation.byte_budget,
            },
            reservation.slot,
        )
        .await
    }

    fn reserve_slot(&self) -> io::Result<OwnedSemaphorePermit> {
        self.slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| open_file_limit_error())
    }

    async fn insert(
        &self,
        handle_id: String,
        backing: FileReadBacking,
        slot: OwnedSemaphorePermit,
    ) -> io::Result<String> {
        let mut handles = self.handles.lock().await;
        if handles.contains_key(&handle_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("file read handle `{handle_id}` already exists"),
            ));
        }
        handles.insert(
            handle_id.clone(),
            Arc::new(ManagedFileReadHandle {
                backing,
                _slot: slot,
            }),
        );
        Ok(handle_id)
    }

    pub(crate) async fn read_block(
        &self,
        handle_id: &str,
        offset: u64,
        len: usize,
    ) -> io::Result<FileReadBlock> {
        validate_read_block_len(len)?;
        let file = {
            let handles = self.handles.lock().await;
            handles
                .get(handle_id)
                .cloned()
                .ok_or_else(|| unknown_handle_error(handle_id))?
        };
        let result = match &file.backing {
            FileReadBacking::File(file) => {
                let file = Arc::clone(file);
                match tokio::task::spawn_blocking(move || read_block_at(&file, offset, len)).await {
                    Ok(result) => result,
                    Err(error) => Err(io::Error::other(format!(
                        "file read task stopped unexpectedly: {error}"
                    ))),
                }
            }
            FileReadBacking::Snapshot { bytes, .. } => Ok(read_snapshot_block(bytes, offset, len)),
        };
        if result.is_err() {
            self.close(handle_id).await;
        }
        result
    }

    pub(crate) async fn close(&self, handle_id: &str) {
        self.handles.lock().await.remove(handle_id);
    }

    pub(crate) async fn close_all(&self) {
        self.handles.lock().await.clear();
    }
}

fn read_snapshot_block(bytes: &Bytes, offset: u64, len: usize) -> FileReadBlock {
    let Some(remaining) = usize::try_from(offset)
        .ok()
        .and_then(|offset| bytes.get(offset..))
    else {
        return FileReadBlock {
            bytes: Vec::new(),
            eof: true,
        };
    };
    let bytes = remaining[..remaining.len().min(len)].to_vec();
    FileReadBlock {
        eof: bytes.len() < len,
        bytes,
    }
}

fn read_block_at(file: &File, offset: u64, len: usize) -> io::Result<FileReadBlock> {
    let mut bytes = vec![0; len];
    let mut bytes_read = 0;
    while bytes_read < len {
        let read_offset = offset.checked_add(bytes_read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "file read offset overflowed")
        })?;
        match read_file_at(file, &mut bytes[bytes_read..], read_offset) {
            Ok(0) => break,
            Ok(read) => bytes_read += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(bytes_read);
    Ok(FileReadBlock {
        eof: bytes_read < len,
        bytes,
    })
}

#[cfg(unix)]
fn read_file_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, bytes, offset)
}

#[cfg(windows)]
fn read_file_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, bytes, offset)
}

fn validate_read_block_len(len: usize) -> io::Result<()> {
    if !(1..=FILE_READ_CHUNK_SIZE).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file read block length must be between 1 and {FILE_READ_CHUNK_SIZE}"),
        ));
    }
    Ok(())
}

fn unknown_handle_error(handle_id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("unknown file read handle `{handle_id}`"),
    )
}

fn open_file_limit_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("at most {MAX_OPEN_FILE_READS} file reads may be open per connection"),
    )
}

fn snapshot_budget_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "verified file reads may retain at most {MAX_RETAINED_SNAPSHOT_BYTES} bytes per connection"
        ),
    )
}

#[cfg(test)]
#[path = "file_read_tests.rs"]
mod tests;
