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
    /// "Copier le lien Dropbox" — copy a shared link for any synced item.
    CopyLink,
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
                CommandKind::CopyLink => labels::label_copy_link(&lang),
            };
            util::alloc_wide(title)
        })
    }

    fn GetIcon(&self, _items: Ref<'_, IShellItemArray>) -> Result<PWSTR> {
        // The app's own icon (`<exe>,0`) on the parent and both children.
        util::guard(Err(E_NOTIMPL.into()), || match util::app_icon_spec() {
            Some(spec) => util::alloc_wide(&spec),
            None => Err(E_NOTIMPL.into()),
        })
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
            let under_root = targets.any_free_up_space || targets.any_hydrate;
            let visible = match kind {
                CommandKind::Parent => under_root,
                // Copy-link targets the single-value clipboard, so only offer it for
                // a single selected item (a multi-select would race N links onto the
                // clipboard and fire N notifications).
                CommandKind::CopyLink => under_root && paths.len() == 1,
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
            // Launch the app once per qualifying item. A mixed selection never
            // dehydrates a `.cloudsc` or hydrates a plain file; copy-link acts on
            // any item under the sync root (file/folder, hydrated or `.cloudsc`).
            for path in util::collect_paths(items.as_ref()) {
                let target = scope::target_for(&path);
                if target == Target::None {
                    continue;
                }
                let action = match kind {
                    CommandKind::CopyLink => "copy_link",
                    CommandKind::FreeUpSpace if target == Target::FreeUpSpace => "free_up_space",
                    CommandKind::Hydrate if target == Target::Hydrate => "hydrate",
                    _ => continue, // Parent, or a child that doesn't match this item
                };
                util::launch_app(action, &path);
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
