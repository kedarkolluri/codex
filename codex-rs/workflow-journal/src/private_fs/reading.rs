//! No-follow private artifact reads with hard bounds or bounded-prefix semantics.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::path::Path;

pub(crate) fn read_private_bounded(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> io::Result<Vec<u8>> {
    let file = open_private_read(path, label)?;
    if file.metadata()?.len() > max_bytes {
        return Err(file_too_large(label, max_bytes));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(file_too_large(label, max_bytes));
    }
    Ok(bytes)
}

pub(crate) fn read_private_prefix(path: &Path, max_bytes: u64, label: &str) -> io::Result<Vec<u8>> {
    let file = open_private_read(path, label)?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn open_private_read(path: &Path, label: &str) -> io::Result<File> {
    super::harden_existing_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
        use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
        use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

        options
            .access_mode(FILE_GENERIC_READ | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    super::validate_regular_file(&file, label)?;
    super::harden_opened_private_file(&file)?;
    Ok(file)
}

fn file_too_large(label: &str, max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("{label} exceeds the {max_bytes}-byte cap"),
    )
}
