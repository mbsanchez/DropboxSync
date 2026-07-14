//! `IClassFactory` that mints the parent `IExplorerCommand`.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::core::{implement, Interface, Result, BOOL, GUID, IUnknown};
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::UI::Shell::IExplorerCommand;

use crate::command::{CommandKind, DropboxSyncCommand};
use crate::{LOCK_COUNT, OBJECT_COUNT};

#[implement(IClassFactory)]
pub struct DropboxSyncCommandFactory;

impl DropboxSyncCommandFactory {
    pub fn new() -> Self {
        OBJECT_COUNT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for DropboxSyncCommandFactory {
    fn drop(&mut self) {
        OBJECT_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IClassFactory_Impl for DropboxSyncCommandFactory_Impl {
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
        let command: IExplorerCommand = DropboxSyncCommand::new(CommandKind::Parent).into();
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
