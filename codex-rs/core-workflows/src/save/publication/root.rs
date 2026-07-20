use std::io;
use std::path::Path;
use std::path::PathBuf;

use cap_std::fs::Dir;
use cap_std::fs::Metadata;
#[cfg(not(windows))]
use cap_std::fs::OpenOptions;

use super::super::WorkflowSaveError;
use super::super::WorkflowSaveRoot;

pub(super) struct PreparedWorkflowRoot {
    pub(super) dir: Dir,
    pub(super) path: PathBuf,
    #[cfg(windows)]
    pub(super) private_acl: bool,
}

pub(super) fn prepare_workflow_root(
    requested_root: &WorkflowSaveRoot,
) -> Result<PreparedWorkflowRoot, WorkflowSaveError> {
    // Only this explicitly trusted boundary is canonicalized. Platform aliases
    // above it (for example macOS `/var`) are therefore harmless, while every
    // fixed registry component below it is opened no-follow through a held
    // directory capability.
    let requested_base = requested_root.trusted_base.as_path();
    let canonical_base =
        std::fs::canonicalize(requested_base).map_err(|source| WorkflowSaveError::Io {
            action: "canonicalize trusted workflow-save boundary",
            path: requested_base.to_path_buf(),
            source,
        })?;
    let mut current = open_ambient_directory_nofollow(&canonical_base).map_err(|source| {
        WorkflowSaveError::Io {
            action: "open trusted workflow-save boundary",
            path: canonical_base.clone(),
            source,
        }
    })?;
    let mut current_path = canonical_base;
    #[cfg(any(unix, windows))]
    let requires_private_permissions = requested_root.requires_private_permissions();

    for &component in requested_root.components() {
        current_path.push(component);
        #[cfg(windows)]
        {
            current = open_or_create_child_directory_nofollow(
                &current,
                component,
                &current_path,
                requires_private_permissions,
            )?;
            continue;
        }
        #[cfg(not(windows))]
        {
            let created = match current.symlink_metadata(component) {
                Ok(metadata) => {
                    validate_directory_metadata(&current_path, &metadata)?;
                    false
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    #[cfg(not(unix))]
                    let builder = cap_std::fs::DirBuilder::new();
                    #[cfg(unix)]
                    let mut builder = cap_std::fs::DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use cap_std::fs::DirBuilderExt;

                        builder.mode(/*mode*/ 0o700);
                    }
                    match current.create_dir_with(component, &builder) {
                        Ok(()) => true,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
                        Err(source) => {
                            return Err(WorkflowSaveError::Io {
                                action: "create private workflow directory",
                                path: current_path,
                                source,
                            });
                        }
                    }
                }
                Err(source) => {
                    return Err(WorkflowSaveError::Io {
                        action: "inspect workflow registry component",
                        path: current_path,
                        source,
                    });
                }
            };

            let metadata =
                current
                    .symlink_metadata(component)
                    .map_err(|source| WorkflowSaveError::Io {
                        action: "inspect admitted workflow registry component",
                        path: current_path.clone(),
                        source,
                    })?;
            validate_directory_metadata(&current_path, &metadata)?;
            let next = open_child_directory_nofollow(&current, component).map_err(|source| {
                WorkflowSaveError::Io {
                    action: "open workflow registry component without following links",
                    path: current_path.clone(),
                    source,
                }
            })?;
            if created {
                set_private_directory_permissions(&next, &current_path)?;
            }
            #[cfg(unix)]
            if requires_private_permissions {
                validate_private_unix_directory(&next, &current_path)?;
            }
            current = next;
        }
    }

    Ok(PreparedWorkflowRoot {
        dir: current,
        path: current_path,
        #[cfg(windows)]
        private_acl: requires_private_permissions,
    })
}

fn validate_directory_metadata(path: &Path, metadata: &Metadata) -> Result<(), WorkflowSaveError> {
    if is_link_or_reparse_point(metadata) || !metadata.is_dir() {
        return Err(WorkflowSaveError::InvalidRoot {
            path: path.to_path_buf(),
            reason: "registry components must be real directories, not links or reparse points"
                .to_string(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_unix_directory(dir: &Dir, path: &Path) -> Result<(), WorkflowSaveError> {
    use cap_std::fs::MetadataExt;

    let metadata = dir.dir_metadata().map_err(|source| WorkflowSaveError::Io {
        action: "inspect private workflow registry component handle",
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: `geteuid` has no preconditions and does not retain pointers.
    let effective_uid = unsafe { libc::geteuid() };
    if !private_unix_directory_attributes_are_safe(metadata.uid(), metadata.mode(), effective_uid) {
        return Err(WorkflowSaveError::InvalidRoot {
            path: path.to_path_buf(),
            reason: "Personal registry components must be owned by the effective user and not writable by group or other users".to_string(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn private_unix_directory_attributes_are_safe(
    owner_uid: u32,
    mode: u32,
    effective_uid: u32,
) -> bool {
    owner_uid == effective_uid && mode & 0o022 == 0
}

#[cfg(unix)]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::other(
            "trusted workflow boundary is not a directory",
        ));
    }
    Ok(Dir::from_std_file(file))
}

#[cfg(windows)]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ADD_SUBDIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_ADD_SUBDIRECTORY | FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    let metadata = file.metadata()?;
    if super::super::filesystem::is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(
            "trusted workflow boundary is not a real directory",
        ));
    }
    Ok(Dir::from_std_file(file))
}

#[cfg(not(any(unix, windows)))]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}

#[cfg(not(windows))]
fn open_child_directory_nofollow(parent: &Dir, component: &str) -> io::Result<Dir> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;

        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let file = parent.open_with(component, &options)?;
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(
            "workflow registry component is not a real directory",
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

#[cfg(windows)]
fn open_or_create_child_directory_nofollow(
    parent: &Dir,
    component: &str,
    path: &Path,
    private_acl: bool,
) -> Result<Dir, WorkflowSaveError> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_IF;
    use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
    use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
    use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
    use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
    use windows_sys::Win32::Foundation::UNICODE_STRING;
    use windows_sys::Win32::Storage::FileSystem::FILE_ADD_FILE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ADD_SUBDIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::System::WindowsProgramming::FILE_CREATED;
    use windows_sys::Win32::System::WindowsProgramming::FILE_OPENED;

    let mut wide_component = std::ffi::OsStr::new(component)
        .encode_wide()
        .collect::<Vec<_>>();
    let component_bytes = wide_component
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| WorkflowSaveError::InvalidRoot {
            path: path.to_path_buf(),
            reason: "registry component name is too long for a native Windows path".to_string(),
        })?;
    let object_name = UNICODE_STRING {
        Length: component_bytes,
        MaximumLength: component_bytes,
        Buffer: wide_component.as_mut_ptr(),
    };
    let object_attributes_size = u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(|_| {
        WorkflowSaveError::InvalidRoot {
            path: path.to_path_buf(),
            reason: "native Windows object attributes are too large".to_string(),
        }
    })?;
    let private_descriptor = private_acl
        .then(super::super::filesystem::PrivateWindowsSecurityDescriptor::new)
        .transpose()
        .map_err(|source| WorkflowSaveError::Io {
            action: "build private workflow-directory security descriptor",
            path: path.to_path_buf(),
            source,
        })?;
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: object_attributes_size,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: private_descriptor
            .as_ref()
            .map_or(std::ptr::null(), |descriptor| descriptor.as_ptr()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut handle = std::ptr::null_mut();

    // SAFETY: all structures match the native ABI and remain alive for the
    // synchronous call. `component` is a fixed single path component and is
    // resolved by the kernel relative to the held parent-directory handle.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_ADD_FILE
                | FILE_ADD_SUBDIRECTORY
                | FILE_READ_ATTRIBUTES
                | FILE_TRAVERSE
                | SYNCHRONIZE
                | if private_acl { READ_CONTROL } else { 0 },
            &object_attributes,
            &mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN_IF,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        let source = super::super::filesystem::nt_status_to_io_error(status);
        if let Ok(metadata) = parent.symlink_metadata(component) {
            validate_directory_metadata(path, &metadata)?;
        }
        return Err(WorkflowSaveError::Io {
            action: "open or create workflow registry component by directory handle",
            path: path.to_path_buf(),
            source,
        });
    }
    if handle.is_null() {
        return Err(WorkflowSaveError::Io {
            action: "open or create workflow registry component by directory handle",
            path: path.to_path_buf(),
            source: io::Error::other("native Windows directory open returned no handle"),
        });
    }

    // SAFETY: successful `NtCreateFile` returned one newly owned handle.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let dir = Dir::from_std_file(file);
    let metadata = dir.dir_metadata().map_err(|source| WorkflowSaveError::Io {
        action: "inspect opened workflow registry component handle",
        path: path.to_path_buf(),
        source,
    })?;
    validate_directory_metadata(path, &metadata)?;
    if private_acl {
        let (action, validation) = match io_status.Information {
            information if information == FILE_CREATED as usize => (
                "verify creation-time private workflow-directory DACL",
                super::super::filesystem::validate_created_private_windows_object(
                    dir.as_raw_handle(),
                ),
            ),
            information if information == FILE_OPENED as usize => (
                "validate existing private workflow-directory DACL",
                super::super::filesystem::validate_existing_private_windows_object(
                    dir.as_raw_handle(),
                ),
            ),
            _ => {
                return Err(WorkflowSaveError::Io {
                    action: "classify native workflow-directory open disposition",
                    path: path.to_path_buf(),
                    source: io::Error::other(
                        "native Windows directory open returned an unexpected disposition",
                    ),
                });
            }
        };
        validation.map_err(|source| WorkflowSaveError::Io {
            action,
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(dir)
}

#[cfg(windows)]
pub(super) fn is_link_or_reparse_point(metadata: &Metadata) -> bool {
    use cap_std::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(super) fn is_link_or_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn set_private_directory_permissions(dir: &Dir, path: &Path) -> Result<(), WorkflowSaveError> {
    use std::os::unix::fs::PermissionsExt;

    dir.try_clone()
        .and_then(|dir| {
            dir.into_std_file()
                .set_permissions(std::fs::Permissions::from_mode(/*mode*/ 0o700))
        })
        .map_err(|source| WorkflowSaveError::Io {
            action: "set private workflow-directory permissions",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(any(unix, windows)))]
fn set_private_directory_permissions(_dir: &Dir, _path: &Path) -> Result<(), WorkflowSaveError> {
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "root_tests.rs"]
mod tests;
