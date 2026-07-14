//! Small COM/OS helpers: string marshaling, item-array parsing, self path, launch.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::core::{Result, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_OUTOFMEMORY, HMODULE};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::Shell::{IShellItemArray, SIGDN_FILESYSPATH};

/// Run a COM entry-point body behind a panic firewall. A panic that reaches the
/// `extern "system"` vtable thunk aborts the process — which for an in-proc shell
/// extension means killing `explorer.exe`. Catch it and return the fail-closed
/// `fallback` instead (e.g. `ECS_HIDDEN`, `E_FAIL`).
pub fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// Allocate a NUL-terminated UTF-16 copy of `s` with `CoTaskMemAlloc`; the shell
/// takes ownership and frees it. Used for `GetTitle`.
pub fn alloc_wide(s: &str) -> Result<PWSTR> {
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        let p = CoTaskMemAlloc(bytes) as *mut u16;
        if p.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        Ok(PWSTR(p))
    }
}

/// Extract the filesystem paths from an `IShellItemArray`. Items without a
/// filesystem path (virtual/library items) are skipped rather than failing.
pub fn collect_paths(items: Option<&IShellItemArray>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(items) = items else {
        return out;
    };
    unsafe {
        let Ok(count) = items.GetCount() else {
            return out;
        };
        for i in 0..count {
            let Ok(item) = items.GetItemAt(i) else {
                continue;
            };
            let Ok(pw) = item.GetDisplayName(SIGDN_FILESYSPATH) else {
                continue;
            };
            if pw.is_null() {
                continue;
            }
            let s = pw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pw.0 as *const c_void));
            if !s.is_empty() {
                out.push(PathBuf::from(s));
            }
        }
    }
    out
}

/// Full path of this DLL (via a code address inside it). Used to register
/// `InprocServer32` and to locate the app exe next to the DLL as a fallback.
pub fn current_module_path() -> PathBuf {
    unsafe {
        let mut hmod = HMODULE::default();
        let addr = current_module_path as *const c_void;
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .is_err()
        {
            return PathBuf::new();
        }
        let mut buf = [0u16; 1024];
        let len = GetModuleFileNameW(Some(hmod), &mut buf);
        if len == 0 {
            return PathBuf::new();
        }
        PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Launch the app with the shell-action args. The exe is resolved from the value
/// the app wrote at registration; falls back to an exe sitting next to the DLL.
///
/// One process per path. The single-instance arg channel carries one `--path`,
/// so a large multi-selection fans out to many short-lived launches; batching
/// several paths into one invocation is a follow-up (DBSYNC-33) if big
/// multi-selects prove common.
pub fn launch_app(action: &str, path: &Path) {
    let Some(exe) = crate::registry::resolve_app_exe(&current_module_path()) else {
        return;
    };
    let _ = Command::new(exe)
        .arg(format!("--action={action}"))
        .arg(format!("--path={}", path.display()))
        .spawn();
}
