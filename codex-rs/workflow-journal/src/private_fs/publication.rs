//! Create-only private artifact publication across supported platforms.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

pub(crate) fn write_private_create_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let destination_name = path
        .file_name()
        .ok_or_else(|| invalid_data("workflow artifact has no file name"))?
        .to_string_lossy();
    let file_name = format!(".{destination_name}-{}.tmp", uuid::Uuid::now_v7());
    let temp_path = path.with_file_name(file_name);
    let write_result = (|| {
        let mut file = open_private_create_new(&temp_path)?;
        ensure_private_permissions(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        publish_private_create_only(&file, &temp_path, path)?;
        super::sync_parent_directory(path)
    })();
    let _ = fs::remove_file(&temp_path);
    write_result
}

#[cfg(unix)]
pub(crate) fn open_private_create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(invalid_data(
            "workflow artifact temp is not a regular, non-symlink file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn open_private_create_new(path: &Path) -> io::Result<File> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    let descriptor = super::windows_acl::private_windows_security_descriptor()?;
    let n_length = u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
        .map_err(|_| io::Error::other("Windows security attributes are too large"))?;
    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: n_length,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide_path.push(0);
    // SAFETY: the path and security descriptor remain live; CREATE_NEW avoids
    // opening an existing attacker-controlled object.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &security_attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the new owned handle is transferred exactly once into `File`.
    let file = unsafe { File::from_raw_handle(handle) };
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(invalid_data(
            "workflow artifact temp is not a regular, non-symlink file",
        ));
    }
    super::windows_acl::ensure_private_windows_dacl(&file)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_private_create_new(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow artifact storage is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn publish_private_create_only(file: &File, temp_path: &Path, path: &Path) -> io::Result<()> {
    let _ = file;
    fs::hard_link(temp_path, path)
}

#[cfg(windows)]
fn publish_private_create_only(file: &File, _temp_path: &Path, path: &Path) -> io::Result<()> {
    use std::mem::offset_of;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::FILE_LINK_INFORMATION;
    use windows_sys::Wdk::Storage::FileSystem::FileLinkInformation;
    use windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("workflow artifact destination has no parent"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_data("workflow artifact destination has no file name"))?;
    let mut parent_wide = parent.as_os_str().encode_wide().collect::<Vec<_>>();
    parent_wide.push(0);
    // SAFETY: the NUL-terminated path remains live; OPEN_REPARSE_POINT pins
    // the exact parent directory used as the relative hard-link root.
    let parent_handle = unsafe {
        CreateFileW(
            parent_wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if parent_handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the new owned handle is transferred exactly once into `File`.
    let parent_file = unsafe { File::from_raw_handle(parent_handle) };
    let parent_metadata = parent_file.metadata()?;
    if is_link_or_reparse_point(&parent_metadata) || !parent_metadata.is_dir() {
        return Err(invalid_data(
            "workflow artifact parent is not a regular, non-reparse directory",
        ));
    }

    let name = file_name.encode_wide().collect::<Vec<_>>();
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| invalid_data("workflow artifact file name is too long"))?;
    let header_len = offset_of!(FILE_LINK_INFORMATION, FileName);
    let buffer_len = header_len
        .checked_add(name_bytes as usize)
        .ok_or_else(|| invalid_data("workflow artifact link request is too large"))?;
    let words = buffer_len.div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let information = buffer.as_mut_ptr().cast::<FILE_LINK_INFORMATION>();
    // SAFETY: the aligned buffer contains the fixed header plus exact UTF-16 name.
    unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = parent_file.as_raw_handle();
        (*information).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            name.len(),
        );
    }
    let mut io_status = IO_STATUS_BLOCK::default();
    let request_len = u32::try_from(buffer_len)
        .map_err(|_| invalid_data("workflow artifact link request is too large"))?;
    // SAFETY: both handles and the aligned request buffer remain live;
    // ReplaceIfExists=false makes publication create-only.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut io_status,
            information.cast(),
            request_len,
            FileLinkInformation,
        )
    };
    if status < 0 {
        // SAFETY: every NTSTATUS can be translated to a Win32 error code.
        return Err(super::windows_acl::windows_error(unsafe {
            RtlNtStatusToDosError(status)
        }));
    }

    let published = open_published_regular_file(path)?;
    super::windows_acl::ensure_private_windows_dacl(&published)?;
    if windows_file_identity(file)? != windows_file_identity(&published)? {
        return Err(invalid_data(
            "workflow artifact publication changed object identity",
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn publish_private_create_only(_file: &File, _temp_path: &Path, _path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow artifact publication is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn open_published_regular_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(invalid_data(
            "workflow artifact is not a regular, non-reparse file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file handle is live and `information` is valid output storage.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
}

#[cfg(unix)]
pub(crate) fn ensure_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(invalid_data(
            "workflow artifact permissions are not owner-private",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn ensure_private_permissions(file: &File) -> io::Result<()> {
    super::windows_acl::ensure_private_windows_dacl(file)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn ensure_private_permissions(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow artifact storage is unsupported on this platform",
    ))
}

#[cfg(windows)]
pub(crate) fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
