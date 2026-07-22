//! Executor-local immutable file capture bound to one opened filesystem object.

use std::io;

use bytes::Bytes;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

use crate::regular_file;

/// Implementation-owned ceiling independent of the caller-provided bound.
pub(crate) const MAX_VERIFIED_FILE_CAPTURE_BYTES: u64 = 16 * 1024 * 1024;

/// Captures one bounded value from a no-follow handle and verifies that `path` still names the
/// same stable object afterward.
///
/// The repeated handle-state and byte checks detect observed mutation, but ordinary mutable files
/// do not provide a portable transaction against a precisely coordinated adversarial writer.
pub(crate) async fn capture(path: &PathUri, max_bytes: u64) -> io::Result<Bytes> {
    let native_path = path.to_abs_path()?;
    let max_bytes = max_bytes.min(MAX_VERIFIED_FILE_CAPTURE_BYTES);
    let mut opened = OpenVerifiedFile::open(&native_path, max_bytes).await?;
    opened
        .require_canonical_path(path, &native_path, max_bytes)
        .await?;
    let bytes = opened.read_stable(max_bytes).await?;
    opened.verify_path(path, &native_path, max_bytes).await?;
    Ok(Bytes::from(bytes))
}

struct OpenVerifiedFile {
    file: File,
    state: FileState,
}

impl OpenVerifiedFile {
    async fn open(native_path: &AbsolutePathBuf, max_bytes: u64) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        configure_no_follow(&mut options);
        let file = options
            .open(native_path.as_path())
            .await
            .map_err(map_no_follow_open_error)?;
        if !regular_file::is_disk_file(&file) {
            return Err(not_regular_file_error());
        }
        let metadata = file.metadata().await?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
            return Err(not_regular_file_error());
        }
        let state = file_state(&file, &metadata)?;
        require_within_limit(state.len, max_bytes)?;
        Ok(Self { file, state })
    }

    async fn read_stable(&mut self, max_bytes: u64) -> io::Result<Vec<u8>> {
        let first = read_from_start(&mut self.file, max_bytes).await?;
        self.require_unchanged(first.len()).await?;

        let second = read_from_start(&mut self.file, max_bytes).await?;
        self.require_unchanged(second.len()).await?;
        if first != second {
            return Err(file_changed_error());
        }
        Ok(first)
    }

    #[cfg(not(windows))]
    async fn require_canonical_path(
        &self,
        path: &PathUri,
        native_path: &AbsolutePathBuf,
        max_bytes: u64,
    ) -> io::Result<()> {
        let _ = self;
        validate_path(path, native_path, max_bytes).await
    }

    #[cfg(windows)]
    async fn require_canonical_path(
        &self,
        path: &PathUri,
        _native_path: &AbsolutePathBuf,
        _max_bytes: u64,
    ) -> io::Result<()> {
        let canonical = canonical_path_from_handle(&self.file)?;
        require_canonical_path(path, canonical)
    }

    async fn verify_path(
        &self,
        path: &PathUri,
        native_path: &AbsolutePathBuf,
        max_bytes: u64,
    ) -> io::Result<()> {
        let verification = Self::open(native_path, max_bytes).await?;
        verification
            .require_canonical_path(path, native_path, max_bytes)
            .await?;
        if self.state != verification.state {
            return Err(file_changed_error());
        }
        Ok(())
    }

    async fn require_unchanged(&self, bytes_read: usize) -> io::Result<()> {
        let metadata = self.file.metadata().await?;
        let state = file_state(&self.file, &metadata)?;
        if state != self.state || bytes_read as u64 != state.len {
            return Err(file_changed_error());
        }
        Ok(())
    }
}

async fn read_from_start(file: &mut File, max_bytes: u64) -> io::Result<Vec<u8>> {
    file.rewind().await?;
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::new();
    file.take(read_limit).read_to_end(&mut bytes).await?;
    require_within_limit(bytes.len() as u64, max_bytes)?;
    Ok(bytes)
}

#[cfg(not(windows))]
async fn validate_path(
    path: &PathUri,
    native_path: &AbsolutePathBuf,
    max_bytes: u64,
) -> io::Result<()> {
    let metadata = tokio::fs::symlink_metadata(native_path.as_path()).await?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(not_regular_file_error());
    }
    require_within_limit(metadata.len(), max_bytes)?;
    let canonical = tokio::fs::canonicalize(native_path.as_path())
        .await
        .and_then(PathUri::from_host_native_path)?;
    require_canonical_path(path, canonical)
}

fn require_canonical_path(path: &PathUri, canonical: PathUri) -> io::Result<()> {
    if canonical == *path {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("verified file path resolves to {canonical}"),
    ))
}

fn require_within_limit(size: u64, max_bytes: u64) -> io::Result<()> {
    if size > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("verified file exceeds the {max_bytes}-byte limit"),
        ));
    }
    Ok(())
}

fn not_regular_file_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "verified file is not a regular, non-symlink file",
    )
}

#[cfg(unix)]
fn map_no_follow_open_error(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(libc::ELOOP) {
        return not_regular_file_error();
    }
    error
}

#[cfg(not(unix))]
fn map_no_follow_open_error(error: io::Error) -> io::Error {
    error
}

fn file_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "verified file changed while being captured",
    )
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileState {
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
fn file_state(_file: &File, metadata: &std::fs::Metadata) -> io::Result<FileState> {
    use std::os::unix::fs::MetadataExt;

    Ok(FileState {
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
struct FileState {
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
fn file_state(file: &File, _metadata: &std::fs::Metadata) -> io::Result<FileState> {
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
            "verified file reported a negative size",
        )
    })?;
    Ok(FileState {
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

#[cfg(windows)]
fn canonical_path_from_handle(file: &File) -> io::Result<PathUri> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED;
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    use windows_sys::Win32::Storage::FileSystem::VOLUME_NAME_DOS;

    let mut path = vec![0_u16; 512];
    loop {
        let capacity = u32::try_from(path.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "verified file path is too long")
        })?;
        // SAFETY: `file` keeps the handle alive and `path` exposes `capacity` writable elements.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as HANDLE,
                path.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < path.len() {
            path.truncate(length);
            let path = PathBuf::from(OsString::from_wide(&path));
            let path = AbsolutePathBuf::from_absolute_path_checked(path)?;
            return Ok(PathUri::from_abs_path(&path));
        }
        path.resize(
            length.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "verified file path is too long")
            })?,
            0,
        );
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileState {
    len: u64,
}

#[cfg(not(any(unix, windows)))]
fn file_state(_file: &File, _metadata: &std::fs::Metadata) -> io::Result<FileState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "verified file capture is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ)
        .security_qos_flags(SECURITY_IDENTIFICATION);
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
#[path = "verified_file_capture_tests.rs"]
mod tests;
