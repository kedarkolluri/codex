//! Private, durable filesystem primitives for workflow-run artifacts.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;

mod publication;
mod reading;
#[cfg(windows)]
mod windows_acl;

pub(crate) use publication::ensure_private_permissions;
pub(crate) use publication::is_link_or_reparse_point;
pub(crate) use publication::open_private_create_new;
pub(crate) use publication::write_private_create_only;
pub(crate) use reading::open_private_read;
pub(crate) use reading::read_private_bounded;
pub(crate) use reading::read_private_prefix;
#[cfg(all(windows, test))]
pub(crate) use windows_acl::LocalSecurityDescriptor;
#[cfg(all(windows, test))]
pub(crate) use windows_acl::ensure_private_windows_dacl;
#[cfg(windows)]
pub(crate) use windows_acl::harden_private_windows_dacl;
#[cfg(windows)]
pub(crate) use windows_acl::private_windows_security_descriptor;
#[cfg(all(windows, test))]
pub(crate) use windows_acl::windows_descriptor_dacl;

#[derive(Clone, Copy)]
pub(crate) enum PrivateFileOpenMode {
    CreateIfMissing,
    Existing,
}

#[derive(Clone, Copy)]
pub(crate) enum PrivateDirectoryOpenMode {
    CreateIfMissing,
    Existing,
}

pub(crate) fn create_run_directory_tree(
    workflows_root: &Path,
    runs_root: &Path,
    run_dir: &Path,
) -> io::Result<()> {
    if runs_root.parent() != Some(workflows_root) || run_dir.parent() != Some(runs_root) {
        return Err(invalid_data("workflow run directory hierarchy is invalid"));
    }
    prepare_private_directories(
        [workflows_root, runs_root, run_dir],
        PrivateDirectoryOpenMode::CreateIfMissing,
    )
}

pub(crate) fn prepare_private_directories<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    mode: PrivateDirectoryOpenMode,
) -> io::Result<()> {
    for path in paths {
        prepare_private_directory(path, mode)?;
    }
    Ok(())
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    reject_non_regular_target(path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_data("workflow artifact has no file name"))?
        .to_string_lossy();
    let temp_path = path.with_file_name(format!(".{file_name}-{}.tmp", uuid::Uuid::now_v7()));
    let write_result = (|| {
        let mut file = open_private_create_new(&temp_path)?;
        ensure_private_permissions(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        replace_file(&temp_path, path)?;
        ensure_private_permissions(&file)?;
        sync_parent_directory(path)
    })();
    let _ = fs::remove_file(&temp_path);
    write_result
}

pub(crate) fn open_private_for_append(path: &Path) -> io::Result<File> {
    prepare_private_file(path, PrivateFileOpenMode::CreateIfMissing)?;
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        options
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_regular_file(&file, "workflow journal")?;
    harden_opened_private_file(&file)?;
    Ok(file)
}

pub(crate) fn open_private_read_write(
    path: &Path,
    mode: PrivateFileOpenMode,
    label: &str,
) -> io::Result<File> {
    prepare_private_file(path, mode)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    validate_regular_file(&file, label)?;
    harden_opened_private_file(&file)?;
    Ok(file)
}

pub(crate) fn prepare_private_file(path: &Path, mode: PrivateFileOpenMode) -> io::Result<()> {
    if matches!(mode, PrivateFileOpenMode::CreateIfMissing) {
        match write_private_create_only(path, &[]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    } else if !path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "private workflow artifact does not exist",
        ));
    }
    harden_private_path(path)?;
    Ok(())
}

pub(crate) fn harden_existing_file(path: &Path) -> io::Result<()> {
    prepare_private_file(path, PrivateFileOpenMode::Existing)
}

#[cfg(unix)]
fn harden_private_path(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_regular_file(&file, "workflow artifact")?;
    harden_opened_private_file(&file)
}

#[cfg(windows)]
fn harden_private_path(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    validate_regular_file(&file, "workflow artifact")?;
    harden_opened_private_file(&file)
}

#[cfg(not(any(unix, windows)))]
fn harden_private_path(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow artifact storage is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn harden_opened_private_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_data(
            "workflow artifact is not owned by the effective user",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
    }
    ensure_private_permissions(file)
}

#[cfg(windows)]
fn harden_opened_private_file(file: &File) -> io::Result<()> {
    harden_private_windows_dacl(file)
}

#[cfg(not(any(unix, windows)))]
fn harden_opened_private_file(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow artifact storage is unsupported on this platform",
    ))
}

fn validate_regular_file(file: &File, label: &str) -> io::Result<()> {
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(invalid_data(format!(
            "{label} is not a regular, non-symlink file"
        )));
    }
    Ok(())
}

pub(crate) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("workflow artifact has no parent directory"))?;
    sync_directory(parent)
}

#[cfg(unix)]
fn prepare_private_directory(path: &Path, mode: PrivateDirectoryOpenMode) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    let created = match mode {
        PrivateDirectoryOpenMode::CreateIfMissing => match builder.create(path) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        },
        PrivateDirectoryOpenMode::Existing => false,
    };
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_data(
            "workflow run directory is not owned by the effective user",
        ));
    }
    let mode_changed = metadata.permissions().mode() & 0o777 != 0o700;
    if mode_changed {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    if created || mode_changed {
        directory.sync_all()?;
    }
    if created {
        sync_parent_directory(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_private_directory(path: &Path, mode: PrivateDirectoryOpenMode) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    let descriptor = private_windows_security_descriptor()?;
    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| io::Error::other("Windows security attributes are too large"))?,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide_path.push(0);
    // SAFETY: the path is NUL-terminated and the private security descriptor
    // remains live for the synchronous directory-creation call.
    let created = match mode {
        PrivateDirectoryOpenMode::CreateIfMissing => {
            if unsafe { CreateDirectoryW(wide_path.as_ptr(), &security_attributes) } != 0 {
                true
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != i32::try_from(ERROR_ALREADY_EXISTS).ok() {
                    return Err(error);
                }
                false
            }
        }
        PrivateDirectoryOpenMode::Existing => false,
    };
    let directory = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | FILE_TRAVERSE | READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(invalid_data(
            "workflow run directory is not a regular, non-reparse directory",
        ));
    }
    harden_private_windows_dacl(&directory)?;
    if created {
        sync_parent_directory(path)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn prepare_private_directory(_path: &Path, _mode: PrivateDirectoryOpenMode) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private workflow directory storage is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL) | Some(libc::ENOTSUP)
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Windows does not provide a portable directory-fsync equivalent. File
    // replacement uses MOVEFILE_WRITE_THROUGH below.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let mut source = source.as_os_str().encode_wide().collect::<Vec<_>>();
    source.push(0);
    let mut destination = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    destination.push(0);
    // SAFETY: both paths are NUL-terminated for this synchronous call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

fn reject_non_regular_target(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse_point(&metadata) => Ok(()),
        Ok(_) => Err(invalid_data(
            "workflow artifact target is not a regular, non-symlink file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[path = "private_fs_tests.rs"]
mod tests;
