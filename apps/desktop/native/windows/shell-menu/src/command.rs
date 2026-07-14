//! The `IExplorerCommand` object — one struct, three roles (parent + 2 children).

use std::sync::atomic::Ordering;

use windows::core::{implement, Ref, Result, BOOL, GUID, PWSTR};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, ECF_DEFAULT,
    ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN,
};

use crate::enumerator::SubCommands;
use crate::scope::{self, Target};
use crate::{labels, util, OBJECT_COUNT};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// The "DropboxSync" flyout that holds the children.
    Parent,
    /// "Désynchroniser" — dehydrate a synced file/folder to free space.
    FreeUpSpace,
    /// "Synchroniser sur le disque" — hydrate a `.cloudsc` placeholder.
    Hydrate,
}

#[implement(IExplorerCommand)]
pub struct DropboxSyncCommand {
    kind: CommandKind,
}

impl DropboxSyncCommand {
    pub fn new(kind: CommandKind) -> Self {
        OBJECT_COUNT.fetch_add(1, Ordering::SeqCst);
        Self { kind }
    }
}

impl Drop for DropboxSyncCommand {
    fn drop(&mut self) {
        OBJECT_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IExplorerCommand_Impl for DropboxSyncCommand_Impl {
    fn GetTitle(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        let kind = self.kind;
        util::guard(Err(E_FAIL.into()), move || {
            let lang = labels::system_ui_language();
            let title = match kind {
                CommandKind::Parent => "DropboxSync", // brand — untranslated
                CommandKind::FreeUpSpace => labels::label_free_up_space(&lang),
                CommandKind::Hydrate => labels::label_sync_to_disk(&lang),
            };
            util::alloc_wide(title)
        })
    }

    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        // No custom icon (optional in the contract); a follow-up may add branding.
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(&self, items: Ref<'_, IShellItemArray>, _slow: BOOL) -> Result<u32> {
        let kind = self.kind;
        // Fail closed (hidden) on any panic — GetState runs on Explorer's UI thread.
        util::guard(Ok(ECS_HIDDEN.0 as u32), move || {
            let paths = util::collect_paths(items.as_ref());
            let targets = scope::classify(&paths);
            let visible = match kind {
                CommandKind::Parent => targets.any_free_up_space || targets.any_hydrate,
                CommandKind::FreeUpSpace => targets.any_free_up_space,
                CommandKind::Hydrate => targets.any_hydrate,
            };
            Ok(if visible {
                ECS_ENABLED.0 as u32
            } else {
                ECS_HIDDEN.0 as u32
            })
        })
    }

    fn Invoke(&self, items: Ref<'_, IShellItemArray>, _bind_ctx: Ref<'_, IBindCtx>) -> Result<()> {
        let kind = self.kind;
        util::guard(Ok(()), move || {
            let (action, want): (&str, Target) = match kind {
                CommandKind::FreeUpSpace => ("free_up_space", Target::FreeUpSpace),
                CommandKind::Hydrate => ("hydrate", Target::Hydrate),
                CommandKind::Parent => return Ok(()),
            };
            // Only act on the items that actually qualify for this child, so a
            // mixed selection never dehydrates a `.cloudsc` or hydrates a plain file.
            for path in util::collect_paths(items.as_ref()) {
                if scope::target_for(&path) == want {
                    util::launch_app(action, &path);
                }
            }
            Ok(())
        })
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(match self.kind {
            CommandKind::Parent => (ECF_HASSUBCOMMANDS.0 | ECF_DEFAULT.0) as u32,
            _ => ECF_DEFAULT.0 as u32,
        })
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        let kind = self.kind;
        util::guard(Err(E_FAIL.into()), move || match kind {
            CommandKind::Parent => Ok(SubCommands::new().into()),
            _ => Err(E_NOTIMPL.into()),
        })
    }
}
