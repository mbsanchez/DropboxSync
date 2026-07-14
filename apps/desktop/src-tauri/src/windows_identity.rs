//! DBSYNC-58: detect whether the running process has a **package identity** (from
//! the sparse package — see `native/windows/sparse-package/`). The Cloud Files API
//! shell integration (DBSYNC-57) needs identity for the WinRT
//! `StorageProviderSyncRootManager.Register`; without it, those features must
//! no-op. During plain `cargo run`/dev without the sparse package installed, this
//! returns false and the app runs normally.
#![cfg(windows)]

use windows_sys::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

/// True if the process is running with a package identity (a sparse/MSIX package
/// is installed and associated with this exe).
pub(crate) fn has_package_identity() -> bool {
    let mut len: u32 = 0;
    // SAFETY: with a null buffer and length 0 the API writes nothing; it only
    // distinguishes "no package" from "buffer too small" via the return code.
    let rc = unsafe { GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) };
    rc != APPMODEL_ERROR_NO_PACKAGE
}
