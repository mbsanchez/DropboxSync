# Windows Explorer context menu (COM `IExplorerCommand`)

An in-process COM server (`dropbox_sync_shell_menu.dll`) that adds a cascading
**"DropboxSync"** right-click menu in File Explorer:

| Child | Shown on | Launches |
|---|---|---|
| **Désynchroniser** (free up space) | regular files/folders **under the sync root** | `app.exe --action=free_up_space --path="<item>"` |
| **Synchroniser sur le disque** (hydrate) | `.cloudsc` placeholders **under the sync root** | `app.exe --action=hydrate --path="<item>"` |

Nothing is shown outside the sync root. Labels follow the system UI language
(fr/es/de/it/pt, English fallback). This DLL is the native surface for
DBSYNC-33; the actions themselves are handled by the running app
(`src-tauri/src/shell_actions.rs` → `cloudsc_ops`).

## How it works

- One CLSID `{FBF4F890-5407-47BF-BE25-F5B2595FA839}` registered per-user (HKCU,
  no admin) on `AllFilesystemObjects` (files, folders and drives) as an
  `IExplorerCommand` verb with a `command`/`DelegateExecute` subkey +
  `MultiSelectModel` — required for the verb to surface on Windows 11 (a bare
  `ExplorerCommandHandler` is culled and never appears).
- Per-item visibility is decided in `IExplorerCommand::GetState` by reading the
  app's `overlay_state.json` (`%LOCALAPPDATA%\DropboxSyncDesktop\`) for the sync
  root and checking the item's extension. The sync root is cached and only
  re-read when the status file's mtime changes, so `GetState` stays cheap (it
  runs on Explorer's UI thread).
- Selecting a child spawns the app exe with the shell-action args. The exe path
  comes from `HKCU\Software\DropboxSyncDesktop\ShellExt\ExePath` (written by the
  app at startup), falling back to an exe next to the DLL.

## Registration

Two equivalent paths write the same HKCU keys:

- **App (production):** on startup the app calls
  `windows_shell_menu::sync_shell_menu_registration()`, which registers the
  handler **iff** `dropbox_sync_shell_menu.dll` sits next to the exe, and removes
  the legacy `.cloudsc` flyout to avoid a duplicate menu. If the DLL is absent it
  no-ops and the legacy flyout stays.
- **regsvr32 (standalone/dev):** `DllRegisterServer` / `DllUnregisterServer`
  write/remove the same keys per-user:
  ```
  regsvr32 /s dropbox_sync_shell_menu.dll        # register
  regsvr32 /u /s dropbox_sync_shell_menu.dll      # unregister
  ```

## Build & test (dev)

```bash
# 1) build the DLL and copy it next to the release exe
npm --workspace apps/desktop run build:shell-menu

# 2) build + launch the app (registers the handler, writes ExePath)
npm --workspace apps/desktop run dev:win

# 3) sign in and pick a sync folder so overlay_state.json exists (gives the
#    extension the sync root)

# 4) reload Explorer so it picks up the DLL
taskkill /f /im explorer.exe & start explorer
```

Then right-click:
- a regular file/folder under the sync root → **DropboxSync ▸ Désynchroniser**
- a `.cloudsc` under the root → **DropboxSync ▸ Synchroniser sur le disque**
- anything outside the root → no DropboxSync menu

The DLL is a separate crate and is **not** part of `cargo build`/`npm run build`
of the app.

## Notes / limitations

- **Windows 11:** classic `IExplorerCommand` handlers appear under **"Show more
  options"**, not the new compact menu. The compact menu requires shipping the
  handler in an **MSIX sparse package** — out of scope for this slice.
- **DLL file lock:** once Explorer loads the DLL it holds the file open;
  rebuild/redeploy needs Explorer restarted (`DllCanUnloadNow` returns `S_OK`
  only when no objects are live).
- **Bundling:** wiring the DLL into the Tauri installer so it ships next to the
  exe on end-user machines is a follow-up; today it is built/copied by
  `build:shell-menu` (dev) or `regsvr32` (manual).
