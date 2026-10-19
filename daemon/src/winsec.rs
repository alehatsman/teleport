//! Windows security plumbing for reaching `anchord` over a named pipe.
//!
//! Three questions, each one system call deep: who is this process's user,
//! what is that user's SID as a string, and who owns the kernel object
//! behind a handle. They are what anchor's Windows transport contract asks a
//! client to check (anchor `docs/04-protocol.md`, "Windows: a named pipe"),
//! and nothing else here has a reason to exist.
//!
//! Every `unsafe` block in the anchor client's Windows arm lives in this
//! file, so the reasoning for each can be read in one place. SIDs leave as
//! owned bytes: a SID has one binary encoding, so byte equality is what
//! `EqualSid` computes, and an owned copy cannot outlive the buffer it was
//! read from.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The user SID on this process's token, as bytes.
pub(crate) fn current_user_sid() -> io::Result<Vec<u8>> {
    // SAFETY: `GetCurrentProcess` takes nothing and returns a pseudo-handle
    // that needs no closing.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let process = unsafe { GetCurrentProcess() };
    let mut raw: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is valid for the call; `raw` is a valid out-pointer.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh token handle this function now owns.
    #[expect(unsafe_code, reason = "taking ownership of a Win32 handle")]
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };

    // Sized by asking: the first call fails and reports the length. `u64`
    // storage keeps the buffer aligned for `TOKEN_USER`, which holds a
    // pointer.
    let mut len = 0_u32;
    // SAFETY: a null buffer of length 0 is the documented way to ask for the
    // size; `len` is a valid out-pointer.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut len,
        );
    }
    let mut buf = vec![0_u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes and is 8-aligned.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so `buf` starts with an initialised,
    // suitably aligned `TOKEN_USER` whose `Sid` points into `buf` itself.
    #[expect(unsafe_code, reason = "reading a Win32 out-structure")]
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    sid_bytes(user.User.Sid)
}

/// The owner SID of the kernel object behind `handle` -- for a named pipe,
/// whoever its creator wrote into its security descriptor.
///
/// Readable through a client end: `GENERIC_READ` on a pipe includes
/// `READ_CONTROL`, which is all `OWNER_SECURITY_INFORMATION` needs.
pub(crate) fn kernel_object_owner(handle: RawHandle) -> io::Result<Vec<u8>> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `handle` is valid for the duration of the call; `owner` and
    // `descriptor` are valid out-pointers, and the three not asked for are
    // null as the API allows.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a WIN32_ERROR, which `from_raw_os_error` takes as i32"
        )]
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // Copied out before the descriptor that holds it is freed.
    let bytes = if owner.is_null() {
        Err(io::Error::other("the object has no owner"))
    } else {
        sid_bytes(owner)
    };
    // SAFETY: `descriptor` was allocated by `GetSecurityInfo` with
    // `LocalAlloc`, is freed once, and `owner` points into it and is not
    // used past this line.
    #[expect(unsafe_code, reason = "freeing a Win32 allocation")]
    unsafe {
        LocalFree(descriptor);
    }
    bytes
}

/// `sid` in its string form (`S-1-5-21-...`), as `ConvertSidToStringSidW`
/// writes it -- the form anchor names its pipe with.
pub(crate) fn sid_to_string(sid: &[u8]) -> io::Result<String> {
    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is a SID's bytes (from `sid_bytes`), which is all the
    // call reads; `wide` is a valid out-pointer. The API takes a non-const
    // PSID but does not write through it.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let ok = unsafe { ConvertSidToStringSidW(sid.as_ptr().cast_mut().cast(), &raw mut wide) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0_usize;
    loop {
        // SAFETY: `wide` is a NUL-terminated string the call allocated, and
        // this stops at its terminator, so every read is inside it.
        #[expect(unsafe_code, reason = "reading a Win32 out-string")]
        let at = unsafe { wide.add(len) };
        // SAFETY: as above -- `at` is inside the string, terminator included.
        #[expect(unsafe_code, reason = "reading a Win32 out-string")]
        let unit = unsafe { at.read() };
        if unit == 0 {
            break;
        }
        len += 1;
    }
    // SAFETY: `len` units at `wide` were just walked and are initialised.
    #[expect(unsafe_code, reason = "reading a Win32 out-string")]
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    // SAFETY: allocated by `ConvertSidToStringSidW` with `LocalAlloc`, freed
    // once, not used after.
    #[expect(unsafe_code, reason = "freeing a Win32 allocation")]
    unsafe {
        LocalFree(wide.cast());
    }
    Ok(text)
}

/// Copies the SID at `sid` out as bytes.
fn sid_bytes(sid: PSID) -> io::Result<Vec<u8>> {
    // SAFETY: callers pass a pointer to a valid SID they hold alive.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let len = unsafe { GetLengthSid(sid) } as usize;
    if len == 0 {
        return Err(io::Error::other("not a valid SID"));
    }
    // SAFETY: `GetLengthSid` says the SID occupies `len` bytes at `sid`.
    #[expect(unsafe_code, reason = "reading a Win32 out-structure")]
    let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) };
    Ok(bytes.to_vec())
}

/// A pipe server that looks like anchord to the client's checks: owned by
/// this user, and open to this user only. Test fakes need it because a pipe
/// created with default security is owned by the token's default owner,
/// which for an elevated process -- a CI runner, for one -- is
/// `Administrators`, and the client would rightly refuse it.
#[cfg(test)]
pub(crate) fn owner_only_pipe_server(
    name: &str,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sid = sid_to_string(&current_user_sid()?)?;
    let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated UTF-16; `descriptor` is a valid
    // out-pointer; the size out-pointer may be null.
    #[expect(unsafe_code, reason = "Win32 call; no safe wrapper")]
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` points at a valid descriptor for the duration of
    // the call, which copies what it needs.
    #[expect(unsafe_code, reason = "tokio's raw security-attributes entry point")]
    let server = unsafe {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    // SAFETY: allocated by the SDDL conversion with `LocalAlloc`, freed once,
    // after the only use.
    #[expect(unsafe_code, reason = "freeing a Win32 allocation")]
    unsafe {
        LocalFree(descriptor);
    }
    server
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_process_has_a_user_sid_in_string_form() {
        let sid = super::current_user_sid().expect("our SID");
        assert_eq!(
            sid,
            super::current_user_sid().expect("again"),
            "stable across reads"
        );
        let text = super::sid_to_string(&sid).expect("string form");
        assert!(text.starts_with("S-1-"), "a SID string: {text}");
    }

    /// The fake server's pipe is owned by this user -- read back through the
    /// same call the client's check makes, so a pass here means the check
    /// sees what anchord would show it.
    #[tokio::test(flavor = "current_thread")]
    async fn an_owner_only_pipe_reports_this_user_as_its_owner() {
        use std::os::windows::io::AsRawHandle;

        let name = format!(r"\\.\pipe\tp-winsec-owner-{}", std::process::id());
        let server = super::owner_only_pipe_server(&name).expect("creating the pipe");
        let owner = super::kernel_object_owner(server.as_raw_handle()).expect("owner");
        assert_eq!(owner, super::current_user_sid().expect("our SID"));
    }
}
