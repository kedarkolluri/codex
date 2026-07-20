use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::FILE_CREATE;
use windows_sys::Wdk::Storage::FileSystem::FILE_NON_DIRECTORY_FILE;
use windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT;
use windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT;
use windows_sys::Wdk::Storage::FileSystem::NtCreateFile;
use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::Foundation::UNICODE_STRING;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::WindowsProgramming::FILE_CREATED;

use super::root::PreparedWorkflowRoot;

pub(super) fn create_temp_file(
    root: &PreparedWorkflowRoot,
    name: &str,
) -> io::Result<cap_std::fs::File> {
    let mut wide_name = std::ffi::OsStr::new(name).encode_wide().collect::<Vec<_>>();
    let name_bytes = wide_name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "temp name is too long"))?;
    let object_name = UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: name_bytes,
        Buffer: wide_name.as_mut_ptr(),
    };
    let object_attributes_size = u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
        .map_err(|_| io::Error::other("native Windows object attributes are too large"))?;
    // A Personal temp file receives its final owner and protected DACL in the
    // native creation call, before its directory entry can be opened by name.
    let private_descriptor = root
        .private_acl
        .then(super::super::filesystem::PrivateWindowsSecurityDescriptor::new)
        .transpose()?;
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: object_attributes_size,
        RootDirectory: root.dir.as_raw_handle(),
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: private_descriptor
            .as_ref()
            .map_or(std::ptr::null(), |descriptor| descriptor.as_ptr()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut handle = std::ptr::null_mut();
    // SAFETY: every structure matches the native ABI and remains live for the
    // synchronous call. The fixed temp name is resolved relative to the held,
    // already-validated workflow-directory handle.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_WRITE | DELETE | if root.private_acl { READ_CONTROL } else { 0 },
            &object_attributes,
            &mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(super::super::filesystem::nt_status_to_io_error(status));
    }
    if handle.is_null() {
        return Err(io::Error::other(
            "native Windows temp-file creation returned no handle",
        ));
    }
    // SAFETY: successful `NtCreateFile` returned one newly owned handle.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    if io_status.Information != FILE_CREATED as usize {
        return Err(io::Error::other(
            "native Windows temp-file creation returned an unexpected disposition",
        ));
    }
    Ok(cap_std::fs::File::from_std(file))
}
