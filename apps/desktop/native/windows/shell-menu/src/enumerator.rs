//! `IEnumExplorerCommand` yielding the two child commands. Both are always
//! enumerated; each child's `GetState` decides whether it is shown or hidden.

use std::cell::Cell;
use std::sync::atomic::Ordering;

use windows::core::{implement, Result, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, S_FALSE, S_OK};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand,
};

use crate::command::{CommandKind, DropboxSyncCommand};
use crate::OBJECT_COUNT;

const CHILDREN: [CommandKind; 2] = [CommandKind::FreeUpSpace, CommandKind::Hydrate];

#[implement(IEnumExplorerCommand)]
pub struct SubCommands {
    cursor: Cell<usize>,
}

impl SubCommands {
    pub fn new() -> Self {
        OBJECT_COUNT.fetch_add(1, Ordering::SeqCst);
        Self {
            cursor: Cell::new(0),
        }
    }
}

impl Drop for SubCommands {
    fn drop(&mut self) {
        OBJECT_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IEnumExplorerCommand_Impl for SubCommands_Impl {
    fn Next(
        &self,
        celt: u32,
        command: *mut Option<IExplorerCommand>,
        fetched: *mut u32,
    ) -> HRESULT {
        // Fail closed on any panic — a panic across the extern boundary aborts
        // Explorer.
        crate::util::guard(E_FAIL, || {
            if command.is_null() {
                return E_POINTER;
            }
            if !fetched.is_null() {
                unsafe { *fetched = 0 };
            }
            let mut written: u32 = 0;
            let mut i = self.cursor.get();
            while written < celt && i < CHILDREN.len() {
                let cmd: IExplorerCommand = DropboxSyncCommand::new(CHILDREN[i]).into();
                // `command` is a caller-owned, UNINITIALIZED output buffer. Use
                // `ptr::write` (not `*p = v`) so we don't run `drop_in_place` on
                // the garbage bytes — dropping a niche-optimized
                // `Option<IExplorerCommand>` would Release through a bad vtable
                // and crash Explorer.
                unsafe { std::ptr::write(command.add(written as usize), Some(cmd)) };
                written += 1;
                i += 1;
            }
            self.cursor.set(i);
            if !fetched.is_null() {
                unsafe { *fetched = written };
            }
            // S_OK only when the whole request was satisfied, else S_FALSE.
            if written == celt {
                S_OK
            } else {
                S_FALSE
            }
        })
    }

    fn Skip(&self, celt: u32) -> Result<()> {
        let next = (self.cursor.get() + celt as usize).min(CHILDREN.len());
        self.cursor.set(next);
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.cursor.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        let fresh = SubCommands::new();
        fresh.cursor.set(self.cursor.get());
        Ok(fresh.into())
    }
}
