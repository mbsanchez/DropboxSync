//! Diagnostic probe for the CfAPI status-column integration (DBSYNC-41).
//! Usage: cargo run --example cfapi_probe -- "<sync_folder>" "<a_file_under_it>"
//! Prints the exact HRESULT of every CfAPI call so we can see what fails.

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // Cleanup mode: `cfapi_probe revert <folder> <file...>` reverts placeholders
    // back to normal files and unregisters the sync root.
    if raw.first().map(String::as_str) == Some("revert") {
        let folder = &raw[1];
        unsafe {
            for f in &raw[2..] {
                let fw = to_wide(f);
                match CreateFileW(
                    PCWSTR(fw.as_ptr()),
                    FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    None,
                ) {
                    Ok(h) => {
                        let r = CfRevertPlaceholder(h, CF_REVERT_FLAG_NONE, None);
                        eprintln!("CfRevertPlaceholder {f}: {r:?}");
                        let _ = CloseHandle(h);
                    }
                    Err(e) => eprintln!("CreateFileW {f}: ERR {e:?}"),
                }
            }
            let path = to_wide(folder);
            let u = CfUnregisterSyncRoot(PCWSTR(path.as_ptr()));
            eprintln!("CfUnregisterSyncRoot: {u:?}");
        }
        return;
    }

    let folder = std::env::args().nth(1).expect("arg1 = sync folder");
    let file = std::env::args().nth(2);

    let path = to_wide(&folder);
    let identity = to_wide(&folder);
    let name = to_wide("DropboxSync");
    let version = to_wide("1.0");

    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: identity.as_ptr() as *const core::ffi::c_void,
        SyncRootIdentityLength: (identity.len() * 2) as u32,
        ProviderId: GUID::from_u128(0xEA067F54_0C87_4DC2_9F90_3A632C2AAF9C),
        ..Default::default()
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_ALWAYS_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_NONE,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_CONVERT_TO_UNRESTRICTED,
    };

    unsafe {
        match CfGetPlatformInfo() {
            Ok(info) => eprintln!(
                "CfGetPlatformInfo: OK build={} revision={}",
                info.BuildNumber, info.RevisionNumber
            ),
            Err(e) => eprintln!("CfGetPlatformInfo: ERR {e:?}"),
        }

        let r = CfRegisterSyncRoot(
            PCWSTR(path.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        );
        eprintln!("CfRegisterSyncRoot: {r:?}");

        if let Some(file) = file {
            let fw = to_wide(&file);
            // Convert a NORMAL file to a placeholder → open with CreateFileW
            // (write attributes + backup semantics), NOT CfOpenFileWithOplock
            // (which is for files already under cloud management).
            match CreateFileW(
                PCWSTR(fw.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            ) {
                Ok(h) => {
                    eprintln!("CreateFileW: OK");
                    let c = CfConvertToPlaceholder(
                        h,
                        None,
                        0,
                        CF_CONVERT_FLAG_MARK_IN_SYNC,
                        None,
                        None,
                    );
                    eprintln!("CfConvertToPlaceholder: {c:?}");
                    let p = CfSetPinState(h, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None);
                    eprintln!("CfSetPinState: {p:?}");
                    let s = CfSetInSyncState(
                        h,
                        CF_IN_SYNC_STATE_IN_SYNC,
                        CF_SET_IN_SYNC_FLAG_NONE,
                        None,
                    );
                    eprintln!("CfSetInSyncState: {s:?}");
                    let _ = CloseHandle(h);
                }
                Err(e) => eprintln!("CreateFileW: ERR {e:?}"),
            }
        }
    }
}
