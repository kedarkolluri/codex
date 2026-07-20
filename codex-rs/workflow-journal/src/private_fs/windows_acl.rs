//! Windows owner and protected-DACL enforcement for private workflow state.

use std::fs::File;
use std::io;

pub(crate) fn ensure_private_windows_dacl(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetSecurityDescriptorControl;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::SE_DACL_PROTECTED;

    let mut owner = std::ptr::null_mut();
    let mut actual_acl = std::ptr::null_mut();
    let mut actual_descriptor = std::ptr::null_mut();
    // SAFETY: the live file handle carries READ_CONTROL. Unrequested group and
    // SACL outputs are null; the returned descriptor is wrapped immediately.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut actual_acl,
            std::ptr::null_mut(),
            &mut actual_descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(windows_error(error));
    }
    let actual_descriptor = LocalSecurityDescriptor(actual_descriptor);
    if actual_acl.is_null() {
        return Err(invalid_data(
            "workflow artifact DACL verification returned no ACL",
        ));
    }
    if owner.is_null() || !windows_owner_matches_current_user(owner)? {
        return Err(invalid_data(
            "workflow artifact owner does not match the current user",
        ));
    }

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: `actual_descriptor` owns a live security descriptor and both
    // outputs point to initialized stack values.
    if unsafe { GetSecurityDescriptorControl(actual_descriptor.0, &mut control, &mut revision) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(invalid_data(
            "workflow artifact DACL is not protected from inheritance",
        ));
    }

    validate_private_windows_acl(actual_acl)
}

fn ensure_current_windows_owner(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the live handle carries READ_CONTROL; unrequested outputs are
    // null and the returned descriptor is wrapped immediately.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(windows_error(error));
    }
    let _descriptor = LocalSecurityDescriptor(descriptor);
    if owner.is_null() || !windows_owner_matches_current_user(owner)? {
        return Err(invalid_data(
            "workflow artifact owner does not match the current user",
        ));
    }
    Ok(())
}

pub(crate) fn private_windows_security_descriptor() -> io::Result<LocalSecurityDescriptor> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;

    // Protected Owner Rights, Local System, and built-in Administrators ACEs
    // match Windows private application-data conventions.
    let sddl = windows_sys::core::w!("D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is a static NUL-terminated string and `descriptor`
    // receives one LocalAlloc-owned security descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalSecurityDescriptor(descriptor))
}

pub(crate) fn windows_descriptor_dacl(
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
) -> io::Result<*mut windows_sys::Win32::Security::ACL> {
    use windows_sys::Win32::Security::GetSecurityDescriptorDacl;

    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = std::ptr::null_mut();
    // SAFETY: `descriptor` is live and all outputs point to initialized stack
    // values.
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || acl.is_null() {
        return Err(invalid_data(
            "private workflow security descriptor has no DACL",
        ));
    }
    Ok(acl)
}

pub(crate) fn harden_private_windows_dacl(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
    use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    // Check ownership before exercising WRITE_DAC so a foreign-owned object is
    // rejected without changing its access policy. Already-private objects are
    // verified without an unconditional SetSecurityInfo mutation.
    ensure_current_windows_owner(file)?;
    match ensure_private_windows_dacl(file) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
        Err(error) => return Err(error),
    }
    let descriptor = private_windows_security_descriptor()?;
    let acl = windows_descriptor_dacl(descriptor.0)?;
    // SAFETY: the handle was opened with WRITE_DAC, and the descriptor owns
    // the ACL for the duration of this synchronous update.
    let error = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if error != ERROR_SUCCESS {
        return Err(windows_error(error));
    }
    ensure_private_windows_dacl(file)
}

fn validate_private_windows_acl(acl: *const windows_sys::Win32::Security::ACL) -> io::Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_ALL;
    use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
    use windows_sys::Win32::Security::GetAce;
    use windows_sys::Win32::Security::WinBuiltinAdministratorsSid;
    use windows_sys::Win32::Security::WinCreatorOwnerRightsSid;
    use windows_sys::Win32::Security::WinLocalSystemSid;
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let ace_count = windows_acl_ace_count(acl)?;
    if ace_count != 3 {
        return Err(invalid_data(
            "workflow artifact DACL has unexpected access entries",
        ));
    }
    let mut owner_rights = false;
    let mut local_system = false;
    let mut administrators = false;
    for index in 0..ace_count {
        let mut raw_ace = std::ptr::null_mut();
        // SAFETY: `index` is below the ACL's reported ACE count.
        if unsafe { GetAce(acl, u32::from(index), &mut raw_ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetAce succeeded; the ACE type is checked before its SID is used.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE || ace.Header.AceFlags != 0 {
            return Err(invalid_data(
                "workflow artifact DACL contains a non-private access entry",
            ));
        }
        if ace.Mask & GENERIC_ALL == 0 && ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS {
            return Err(invalid_data(
                "workflow artifact DACL entry does not grant required recovery access",
            ));
        }
        let sid = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
        let matched = if windows_sid_matches(sid, WinCreatorOwnerRightsSid)? {
            if owner_rights {
                false
            } else {
                owner_rights = true;
                true
            }
        } else if windows_sid_matches(sid, WinLocalSystemSid)? {
            if local_system {
                false
            } else {
                local_system = true;
                true
            }
        } else if windows_sid_matches(sid, WinBuiltinAdministratorsSid)? {
            if administrators {
                false
            } else {
                administrators = true;
                true
            }
        } else {
            false
        };
        if !matched {
            return Err(invalid_data(
                "workflow artifact DACL grants access outside the private policy",
            ));
        }
    }
    if owner_rights && local_system && administrators {
        Ok(())
    } else {
        Err(invalid_data(
            "workflow artifact DACL is missing a required private access entry",
        ))
    }
}

fn windows_acl_ace_count(acl: *const windows_sys::Win32::Security::ACL) -> io::Result<u16> {
    use std::mem::size_of;
    use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
    use windows_sys::Win32::Security::AclSizeInformation;
    use windows_sys::Win32::Security::GetAclInformation;

    let mut information = ACL_SIZE_INFORMATION::default();
    let information_size = u32::try_from(size_of::<ACL_SIZE_INFORMATION>())
        .map_err(|_| io::Error::other("Windows ACL size information is too large"))?;
    // SAFETY: `information` has the exact layout and byte length requested.
    if unsafe {
        GetAclInformation(
            acl,
            std::ptr::addr_of_mut!(information).cast(),
            information_size,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    u16::try_from(information.AceCount)
        .map_err(|_| io::Error::other("Windows ACL has too many entries"))
}

fn windows_sid_matches(
    sid: windows_sys::Win32::Security::PSID,
    expected: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE,
) -> io::Result<bool> {
    use windows_sys::Win32::Security::CreateWellKnownSid;
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE;

    let mut expected_sid = [0_u8; SECURITY_MAX_SID_SIZE as usize];
    let mut expected_size = SECURITY_MAX_SID_SIZE;
    // SAFETY: the output buffer has SECURITY_MAX_SID_SIZE bytes.
    if unsafe {
        CreateWellKnownSid(
            expected,
            std::ptr::null_mut(),
            expected_sid.as_mut_ptr().cast(),
            &mut expected_size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both pointers refer to valid SIDs for this call.
    Ok(unsafe { EqualSid(sid, expected_sid.as_mut_ptr().cast()) } != 0)
}

fn windows_owner_matches_current_user(
    owner: windows_sys::Win32::Security::PSID,
) -> io::Result<bool> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::Security::GetTokenInformation;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Security::TOKEN_USER;
    use windows_sys::Win32::Security::TokenUser;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `token` receives one owned process-token handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = WindowsHandle(token);
    let mut required = 0;
    // SAFETY: the null-buffer probe reports the required byte count.
    let _ =
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required) };
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let word_size = size_of::<usize>();
    let required_usize = usize::try_from(required)
        .map_err(|_| io::Error::other("Windows token identity is too large"))?;
    let words = required_usize.div_ceil(word_size);
    let mut buffer = vec![0_usize; words];
    let buffer_bytes = u32::try_from(buffer.len().saturating_mul(word_size))
        .map_err(|_| io::Error::other("Windows token identity is too large"))?;
    // SAFETY: the aligned buffer is at least the probed byte length.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            buffer_bytes,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful query initializes TOKEN_USER at the buffer start.
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: both pointers refer to live valid SIDs.
    Ok(unsafe { EqualSid(owner, token_user.User.Sid) } != 0)
}

pub(super) fn windows_error(error: u32) -> io::Error {
    match i32::try_from(error) {
        Ok(error) => io::Error::from_raw_os_error(error),
        Err(_) => io::Error::other("Windows security operation failed"),
    }
}

pub(crate) struct LocalSecurityDescriptor(
    pub(crate) windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
);

impl LocalSecurityDescriptor {
    pub(crate) fn as_ptr(&self) -> windows_sys::Win32::Security::PSECURITY_DESCRIPTOR {
        self.0
    }
}

struct WindowsHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for WindowsHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // SAFETY: this guard owns one OpenProcessToken handle exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::LocalFree;

        // SAFETY: this guard owns one LocalAlloc descriptor exactly once.
        let _ = unsafe { LocalFree(self.0.cast()) };
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
