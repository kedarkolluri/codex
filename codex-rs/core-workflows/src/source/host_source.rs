//! Host-local workflow source capture bound to one opened filesystem object.

use std::io;

use codex_code_mode_protocol::WORKFLOW_SOURCE_MAX_BYTES;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

use super::WorkflowSourceLoadError;
use super::source_io_error;
use super::source_too_large;

/// Captures one bounded value from a no-follow handle and verifies that the registry pathname
/// still names the same stable object afterward.
///
/// The repeated handle-state and byte checks detect observed mutation, but ordinary mutable files
/// do not provide a portable transaction against a precisely coordinated adversarial writer.
pub(super) async fn read_host_source(path: &PathUri) -> Result<Vec<u8>, WorkflowSourceLoadError> {
    let native_path = path.to_abs_path().map_err(|error| {
        WorkflowSourceLoadError::new(
            path.clone(),
            format!("source is not representable on the host: {error}"),
        )
    })?;
    validate_host_path(path, &native_path).await?;

    let mut opened = OpenHostSource::open(path, &native_path).await?;
    let bytes = opened.read_stable(path).await?;
    opened.verify_path(path, &native_path).await?;
    Ok(bytes)
}

struct OpenHostSource {
    file: File,
    state: HostFileState,
}

impl OpenHostSource {
    async fn open(
        path: &PathUri,
        native_path: &AbsolutePathBuf,
    ) -> Result<Self, WorkflowSourceLoadError> {
        let mut options = OpenOptions::new();
        options.read(true);
        configure_no_follow(&mut options);
        let file = options
            .open(native_path.as_path())
            .await
            .map_err(|error| source_io_error(path, "open source", error))?;
        let metadata = file
            .metadata()
            .await
            .map_err(|error| source_io_error(path, "inspect opened source", error))?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
            return Err(WorkflowSourceLoadError::new(
                path.clone(),
                "opened workflow source is not a regular, non-reparse file",
            ));
        }
        let state = host_file_state(&file, &metadata)
            .map_err(|error| source_io_error(path, "inspect opened source", error))?;
        if state.len > WORKFLOW_SOURCE_MAX_BYTES as u64 {
            return Err(source_too_large(path));
        }
        Ok(Self { file, state })
    }

    async fn read_stable(&mut self, path: &PathUri) -> Result<Vec<u8>, WorkflowSourceLoadError> {
        let first = read_from_start(path, &mut self.file).await?;
        self.require_unchanged(path, first.len()).await?;

        let second = read_from_start(path, &mut self.file).await?;
        self.require_unchanged(path, second.len()).await?;
        if first != second {
            return Err(source_changed(path));
        }
        Ok(first)
    }

    async fn verify_path(
        &self,
        path: &PathUri,
        native_path: &AbsolutePathBuf,
    ) -> Result<(), WorkflowSourceLoadError> {
        validate_host_path(path, native_path).await?;
        let verification = Self::open(path, native_path).await?;
        if self.state != verification.state {
            return Err(source_changed(path));
        }
        Ok(())
    }

    async fn require_unchanged(
        &self,
        path: &PathUri,
        bytes_read: usize,
    ) -> Result<(), WorkflowSourceLoadError> {
        let metadata = self
            .file
            .metadata()
            .await
            .map_err(|error| source_io_error(path, "reinspect opened source", error))?;
        let state = host_file_state(&self.file, &metadata)
            .map_err(|error| source_io_error(path, "reinspect opened source", error))?;
        if state != self.state || bytes_read as u64 != state.len {
            return Err(source_changed(path));
        }
        Ok(())
    }
}

async fn read_from_start(
    path: &PathUri,
    file: &mut File,
) -> Result<Vec<u8>, WorkflowSourceLoadError> {
    file.rewind()
        .await
        .map_err(|error| source_io_error(path, "seek source", error))?;
    let read_limit = (WORKFLOW_SOURCE_MAX_BYTES + 1) as u64;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| source_io_error(path, "read source", error))?;
    if bytes.len() > WORKFLOW_SOURCE_MAX_BYTES {
        return Err(source_too_large(path));
    }
    Ok(bytes)
}

async fn validate_host_path(
    path: &PathUri,
    native_path: &AbsolutePathBuf,
) -> Result<(), WorkflowSourceLoadError> {
    let metadata = tokio::fs::symlink_metadata(native_path.as_path())
        .await
        .map_err(|error| source_io_error(path, "inspect source", error))?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(WorkflowSourceLoadError::new(
            path.clone(),
            "workflow source is not a regular, non-symlink file",
        ));
    }
    if metadata.len() > WORKFLOW_SOURCE_MAX_BYTES as u64 {
        return Err(source_too_large(path));
    }
    let canonical = tokio::fs::canonicalize(native_path.as_path())
        .await
        .and_then(PathUri::from_host_native_path)
        .map_err(|error| source_io_error(path, "resolve source", error))?;
    if canonical != *path {
        return Err(WorkflowSourceLoadError::new(
            path.clone(),
            format!("workflow source now resolves to {canonical}"),
        ));
    }
    Ok(())
}

fn source_changed(path: &PathUri) -> WorkflowSourceLoadError {
    WorkflowSourceLoadError::new(path.clone(), "workflow source changed while being read")
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostFileState {
    len: u64,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    uid: u32,
    gid: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
fn host_file_state(_file: &File, metadata: &std::fs::Metadata) -> io::Result<HostFileState> {
    use std::os::unix::fs::MetadataExt;

    Ok(HostFileState {
        len: metadata.len(),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostFileState {
    len: u64,
    volume_serial_number: u64,
    file_id: [u8; 16],
    created_at: i64,
    modified_at: i64,
    changed_at: i64,
    attributes: u32,
    links: u32,
    delete_pending: u8,
    directory: u8,
}

#[cfg(windows)]
fn host_file_state(file: &File, _metadata: &std::fs::Metadata) -> io::Result<HostFileState> {
    use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileBasicInfo;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;

    // SAFETY: `FileIdInfo` initializes exactly one `FILE_ID_INFO` value.
    let identity = unsafe { query_windows_file_info::<FILE_ID_INFO>(file, FileIdInfo) }?;
    // SAFETY: `FileBasicInfo` initializes exactly one `FILE_BASIC_INFO` value.
    let basic = unsafe { query_windows_file_info::<FILE_BASIC_INFO>(file, FileBasicInfo) }?;
    // SAFETY: `FileStandardInfo` initializes exactly one `FILE_STANDARD_INFO` value.
    let standard =
        unsafe { query_windows_file_info::<FILE_STANDARD_INFO>(file, FileStandardInfo) }?;
    let len = u64::try_from(standard.EndOfFile).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "opened source reported a negative file size",
        )
    })?;
    Ok(HostFileState {
        len,
        volume_serial_number: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
        created_at: basic.CreationTime,
        modified_at: basic.LastWriteTime,
        changed_at: basic.ChangeTime,
        attributes: basic.FileAttributes,
        links: standard.NumberOfLinks,
        delete_pending: standard.DeletePending,
        directory: standard.Directory,
    })
}

#[cfg(windows)]
/// # Safety
///
/// `class` must direct Windows to initialize exactly one value of `T`.
unsafe fn query_windows_file_info<T>(
    file: &File,
    class: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS,
) -> io::Result<T> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut information = MaybeUninit::<T>::uninit();
    let information_size = u32::try_from(std::mem::size_of::<T>()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file information structure is too large",
        )
    })?;
    // SAFETY: `file` keeps the OS handle alive, `information` is valid for `information_size`
    // writable bytes, and the returned value is only assumed initialized after a successful call.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            class,
            information.as_mut_ptr().cast(),
            information_size,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `GetFileInformationByHandleEx` initialized the selected structure.
    Ok(unsafe { information.assume_init() })
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct HostFileState {
    len: u64,
}

#[cfg(not(any(unix, windows)))]
fn host_file_state(_file: &File, _metadata: &std::fs::Metadata) -> io::Result<HostFileState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "verified host source reads are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
#[path = "host_source_tests.rs"]
mod tests;
