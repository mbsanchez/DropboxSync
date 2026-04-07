# Finder Sync extension (macOS shell overlays)

The Tauri app writes **`overlay_state.json`** next to the SQLite database:

- Path: `~/Library/Applications/DropboxSyncDesktop/overlay_state.json`
- Schema: see `src-tauri/src/overlay_state.rs` (`version`, `updated_at`, `sync_folder`, `paths` map with tiers `synced`, `out_of_sync`, `syncing`).

This folder contains a **Finder Sync** implementation (`SyncExtension.swift`) that registers three badge images and assigns them per file using `requestBadgeIdentifier(for:)`.

## Build from the repo (CLI)

From `apps/desktop`:

```bash
npm run build:finder-sync
```

Produces `native/macos/FinderSyncExtension/build/DropboxSyncFinderSync.appex` (and a copy under `build/`). Override configuration with `CONFIGURATION=Debug`. To skip code signing (local only): `CODE_SIGNING_ALLOWED=NO`. On non-macOS hosts the script exits successfully without building.

## Xcode GUI

Open `DropboxSyncFinderSync.xcodeproj` to edit signing (**Signing & Capabilities** → Team) or Swift. Embed the built **`DropboxSyncFinderSync.appex`** into the main app at **`Contents/PlugIns/`**; host app and extension must use the **same development team** for production. `Info.plist.example` mirrors the committed `Info.plist` for reference.

After install, the user may need to enable the extension under **System Settings → Privacy & Security → Extensions → Finder** (wording varies by macOS version).

## Why not pure Tauri?

Finder badge overlays are implemented only via **App Extension** APIs (`FIFinderSync` / `FIFinderSyncController`). The Rust/Tauri process cannot draw those badges itself.
