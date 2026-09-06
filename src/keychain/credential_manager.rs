//! The Windows Credential Manager, through the credential API in `windows-sys`.
//!
//! There is no `security` or `secret-tool` here, and no tool worth shelling out to:
//! `CredWriteW` and friends are four calls away, and `windows-sys` is already a
//! dependency for the access list on `credentials.json`. Nothing new is linked.
//!
//! Each credential is one generic item named `mcpdial:<account>`, so `CredEnumerateW`
//! with a `mcpdial:*` filter finds all of them and touches nothing else. The blob is
//! the JSON as UTF-8 bytes, held in a buffer this process owns; it is never an
//! argument and never reaches another program.

use crate::protocol::{Error, Result};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, FILETIME};
use windows_sys::Win32::Security::Credentials::{
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

pub fn get(service: &str, account: &str) -> Result<Option<String>> {
    let target = wide(&target_name(service, account));
    let mut found: *mut CREDENTIALW = ptr::null_mut();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut found) } == 0 {
        return match io::Error::last_os_error() {
            e if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) => Ok(None),
            e => Err(Error::config(format!(
                "cannot read the credential for {account:?}: {e}"
            ))),
        };
    }
    let _release = Buffer(found.cast());
    // Safe to read: `CredReadW` succeeded, so it wrote a credential we now own,
    // and the blob it points at is as long as the size beside it says.
    let blob = unsafe {
        let cred = &*found;
        std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize)
    };
    String::from_utf8(blob.to_vec())
        .map(Some)
        .map_err(|_| super::not_a_credential(account))
}

pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
    let blob = secret.as_bytes();
    // The API's own ceiling. Saying so beats the flat "the parameter is incorrect"
    // that `CredWriteW` answers an oversized blob with.
    if blob.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
        return Err(Error::config(format!(
            "the credential for {account:?} is {} bytes; the Windows Credential Manager \
             holds at most {CRED_MAX_CREDENTIAL_BLOB_SIZE}",
            blob.len()
        )));
    }
    let mut target = wide(&target_name(service, account));
    let mut user = wide(account);
    let cred = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: ptr::null_mut(),
        // Set by the store, not by us.
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr().cast_mut(),
        // Local, not roamed: a token that follows the account onto another machine
        // is a copy of a secret nobody asked to make.
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: ptr::null_mut(),
        // What Credential Manager shows in its own listing.
        UserName: user.as_mut_ptr(),
    };
    if unsafe { CredWriteW(&cred, 0) } == 0 {
        return Err(Error::config(format!(
            "cannot save the credential for {account:?}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub fn delete(service: &str, account: &str) -> Result<bool> {
    let target = wide(&target_name(service, account));
    if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
        return match io::Error::last_os_error() {
            e if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) => Ok(false),
            e => Err(Error::config(format!(
                "cannot remove the credential for {account:?}: {e}"
            ))),
        };
    }
    Ok(true)
}

pub fn accounts(service: &str) -> Result<Vec<String>> {
    let filter = wide(&format!("{service}:*"));
    let mut count = 0u32;
    let mut found: *mut *mut CREDENTIALW = ptr::null_mut();
    if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut found) } == 0 {
        return match io::Error::last_os_error() {
            e if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) => Ok(Vec::new()),
            e => Err(Error::config(format!("cannot list credentials: {e}"))),
        };
    }
    let _release = Buffer(found.cast());
    let prefix = format!("{service}:");
    let mut names = Vec::new();
    for i in 0..count as usize {
        // Safe to read: `CredEnumerateW` succeeded, so it wrote `count` pointers,
        // each to a credential whose target name is NUL terminated.
        let target = unsafe { from_wide((**found.add(i)).TargetName) };
        names.extend(target.strip_prefix(&prefix).map(str::to_string));
    }
    Ok(names)
}

fn target_name(service: &str, account: &str) -> String {
    format!("{service}:{account}")
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// # Safety
/// `p` must point at a NUL-terminated UTF-16 string.
unsafe fn from_wide(p: *const u16) -> String {
    let mut len = 0;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Whatever the credential API allocated for us, released when it goes out of scope.
struct Buffer(*const std::ffi::c_void);

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { CredFree(self.0) };
    }
}
