use bytes::Bytes;
use codex_utils_path_uri::PathUri;
use tokio::io;
use uuid::Uuid;

use super::map_remote_error;
use super::map_verified_file_read_error;
use crate::ExecServerClient;
use crate::FILE_READ_CHUNK_SIZE;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::FileSystemSandboxContext;
use crate::VerifiedFileRead;
use crate::VerifiedFileReadOptions;
use crate::protocol::FS_OPEN_VERIFIED_METHOD;
use crate::protocol::FS_READ_BLOCK_METHOD;
use crate::protocol::FsCloseParams;
use crate::protocol::FsOpenParams;
use crate::protocol::FsOpenVerifiedParams;
use crate::protocol::FsReadBlockParams;

struct FileReadRegistration {
    client: ExecServerClient,
    handle_id: String,
    runtime: Option<tokio::runtime::Handle>,
    active: bool,
}

pub(super) async fn open(
    client: ExecServerClient,
    path: PathUri,
    sandbox: Option<FileSystemSandboxContext>,
) -> FileSystemResult<FileSystemReadStream> {
    let registration = FileReadRegistration {
        client,
        handle_id: Uuid::new_v4().simple().to_string(),
        runtime: tokio::runtime::Handle::try_current().ok(),
        active: true,
    };
    registration
        .client
        .fs_open(FsOpenParams {
            handle_id: registration.handle_id.clone(),
            path,
            sandbox,
        })
        .await
        .map_err(map_remote_error)?;
    Ok(read_stream(registration, /*expected_size*/ None))
}

pub(super) async fn open_verified(
    client: ExecServerClient,
    path: PathUri,
    options: VerifiedFileReadOptions,
    sandbox: Option<FileSystemSandboxContext>,
) -> FileSystemResult<VerifiedFileRead> {
    let mut registration = FileReadRegistration {
        client,
        handle_id: Uuid::new_v4().simple().to_string(),
        runtime: tokio::runtime::Handle::try_current().ok(),
        active: true,
    };
    let response = match registration
        .client
        .fs_open_verified(FsOpenVerifiedParams {
            handle_id: registration.handle_id.clone(),
            path,
            max_bytes: options.max_bytes,
            sandbox,
        })
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if matches!(error, crate::ExecServerError::Server { .. }) {
                registration.active = false;
            }
            return Err(map_verified_file_read_error(error));
        }
    };
    if response.handle_id != registration.handle_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{FS_OPEN_VERIFIED_METHOD} returned an unexpected handle ID"),
        ));
    }
    if response.size > options.max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{FS_OPEN_VERIFIED_METHOD} returned size {}, maximum is {}",
                response.size, options.max_bytes
            ),
        ));
    }
    Ok(VerifiedFileRead {
        size: response.size,
        stream: read_stream(registration, Some(response.size)),
    })
}

fn read_stream(
    registration: FileReadRegistration,
    expected_size: Option<u64>,
) -> FileSystemReadStream {
    FileSystemReadStream::new(futures::stream::try_unfold(
        Some((registration, 0_u64)),
        move |state| async move {
            let Some((mut registration, offset)) = state else {
                return Ok(None);
            };
            let len = if let Some(expected_size) = expected_size {
                let remaining = expected_size.checked_sub(offset).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{FS_READ_BLOCK_METHOD} offset {offset} exceeds declared size {expected_size}"
                        ),
                    )
                })?;
                if remaining == 0 {
                    if registration
                        .client
                        .fs_close(FsCloseParams {
                            handle_id: registration.handle_id.clone(),
                        })
                        .await
                        .is_ok()
                    {
                        registration.active = false;
                    }
                    return Ok(None);
                }
                remaining.min(FILE_READ_CHUNK_SIZE as u64) as usize
            } else {
                FILE_READ_CHUNK_SIZE
            };
            let response = registration
                .client
                .fs_read_block(FsReadBlockParams {
                    handle_id: registration.handle_id.clone(),
                    offset,
                    len,
                })
                .await
                .map_err(map_remote_error)?;
            let chunk = Bytes::from(response.chunk.into_inner());
            if chunk.len() > FILE_READ_CHUNK_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{FS_READ_BLOCK_METHOD} returned {} bytes, maximum is {}",
                        chunk.len(),
                        FILE_READ_CHUNK_SIZE
                    ),
                ));
            }
            let next_offset = offset.checked_add(chunk.len() as u64).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{FS_READ_BLOCK_METHOD} offset overflowed after {offset} bytes"),
                )
            })?;
            if let Some(expected_size) = expected_size {
                if next_offset > expected_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{FS_READ_BLOCK_METHOD} exceeded the declared {expected_size}-byte capture"
                        ),
                    ));
                }
                if response.eof && next_offset != expected_size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{FS_READ_BLOCK_METHOD} ended after {next_offset} bytes, expected {expected_size}"
                        ),
                    ));
                }
            }
            let reached_expected_size = expected_size == Some(next_offset);
            if response.eof || reached_expected_size {
                if registration
                    .client
                    .fs_close(FsCloseParams {
                        handle_id: registration.handle_id.clone(),
                    })
                    .await
                    .is_ok()
                {
                    registration.active = false;
                }
                return if chunk.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((chunk, None)))
                };
            }
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{FS_READ_BLOCK_METHOD} returned an empty non-terminal block"),
                ));
            }
            Ok(Some((chunk, Some((registration, next_offset)))))
        },
    ))
}

impl Drop for FileReadRegistration {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let client = self.client.clone();
        let handle_id = self.handle_id.clone();
        let runtime = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        if let Some(runtime) = runtime {
            runtime.spawn(async move {
                let _ = client.fs_close(FsCloseParams { handle_id }).await;
            });
        }
    }
}
