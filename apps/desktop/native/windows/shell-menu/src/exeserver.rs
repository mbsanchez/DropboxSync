//! DBSYNC-62: out-of-process COM ExeServer for `desktop3:CloudFilesContextMenus`
//! verbs — the mechanism that surfaces DropboxSync-branded verbs in the Windows 11
//! **compact** context menu (the in-proc DLL only appears under "Show more options").
//!
//! Registers a class factory per verb CLSID with `CoRegisterClassObject`; the shell
//! launches this exe (with `-Embedding`) on demand and groups our verbs into an
//! app-attributed "DropboxSync" flyout automatically. Each verb reuses the shared
//! `IExplorerCommand` impl (`command.rs`) — same classify + `--action` launch as the
//! classic-menu DLL. Requires the sync root to be AUMID-linked to this package (see
//! `cloud_filter::register_winrt`, which sets the AUMID when registering with identity).

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::core::{implement, Interface, BOOL, GUID, IUnknown, Result};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{
    CoInitializeEx, CoRegisterClassObject, CoRevokeClassObject, CoUninitialize, IClassFactory,
    IClassFactory_Impl, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Shell::IExplorerCommand;
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};

use crate::command::{CommandKind, DropboxSyncCommand};
use crate::{LOCK_COUNT, OBJECT_COUNT};

/// The single CloudFiles verb CLSID — a "DropboxSync" PARENT flyout that holds the
/// three child verbs (Libérer / Synchroniser / Copier le lien) via `EnumSubCommands`.
/// One top-level verb (vs three) means the shell shows OUR flyout with OUR icon
/// (`GetIcon`), not the icon-less auto-grouped app-attribution flyout. MUST match a
/// `desktop3:Verb Clsid` + `com:Class Id` in the manifest. NEVER change once shipped.
pub const CLSID_MENU_PARENT: GUID = GUID::from_u128(0x7C3A1E44_9B2D_4F6A_A1E7_2D9C8B5F0A31);

/// The four Shell handler CLSIDs the CloudFiles schema requires before
/// `CloudFilesContextMenus`. We serve them so the extension is "complete", but our
/// object only implements `IExplorerCommand`, so the shell's QueryInterface for a
/// handler interface fails and it skips them (harmless).
const CLSID_HANDLERS: [GUID; 4] = [
    GUID::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001),
    GUID::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0002),
    GUID::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0003),
    GUID::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0004),
];

/// `IClassFactory` minting one leaf `IExplorerCommand` of a fixed kind.
#[implement(IClassFactory)]
struct LeafFactory {
    kind: CommandKind,
}

impl LeafFactory {
    fn new(kind: CommandKind) -> Self {
        OBJECT_COUNT.fetch_add(1, Ordering::SeqCst);
        Self { kind }
    }
}

impl Drop for LeafFactory {
    fn drop(&mut self) {
        OBJECT_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IClassFactory_Impl for LeafFactory_Impl {
    fn CreateInstance(
        &self,
        outer: windows::core::Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        if ppvobject.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppvobject = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let command: IExplorerCommand = DropboxSyncCommand::new(self.kind).into();
        unsafe { command.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        if flock.as_bool() {
            LOCK_COUNT.fetch_add(1, Ordering::SeqCst);
        } else {
            LOCK_COUNT.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Register all verb + handler class factories and pump the STA message loop. The
/// process stays alive as a reusable COM server.
pub fn run_cloudfiles_exe_server() -> Result<()> {
    // One "DropboxSync" parent verb (its EnumSubCommands yields the three children) +
    // the four inert handlers (kind irrelevant — skipped via E_NOINTERFACE).
    let mut registrations: Vec<(GUID, CommandKind)> = vec![(CLSID_MENU_PARENT, CommandKind::Parent)];
    for h in CLSID_HANDLERS {
        registrations.push((h, CommandKind::FreeUpSpace));
    }

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let mut factories: Vec<IClassFactory> = Vec::new();
        let mut cookies: Vec<u32> = Vec::new();
        for (clsid, kind) in registrations {
            let factory: IClassFactory = LeafFactory::new(kind).into();
            let cookie =
                CoRegisterClassObject(&clsid, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE)?;
            factories.push(factory);
            cookies.push(cookie);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for c in cookies {
            let _ = CoRevokeClassObject(c);
        }
        CoUninitialize();
    }
    Ok(())
}
