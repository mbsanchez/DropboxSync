# Windows shell icon overlays

The desktop app writes the same **`overlay_state.json`** as on macOS, under:

`%LOCALAPPDATA%\DropboxSyncDesktop\overlay_state.json`

(see `src-tauri/src/storage/db.rs` → `app_data_dir` on Windows).

## What to implement

Comparable to Dropbox / OneDrive / odrive, Explorer overlays use **COM** shell extensions implementing **`IShellIconOverlayIdentifier`** (often one **CLSID** per overlay tier). Registration goes under:

`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ShellIconOverlayIdentifiers\`

**Important:** Windows reserves a **small global pool** of overlay slots (on the order of 15) shared by all software. Names are often prefixed with spaces to influence sort order; conflicts with other products are possible.

## Suggested integration

1. Build a **64-bit in-proc COM DLL** (Rust with the `windows` crate or MSVC C++) that:
   - Reads `overlay_state.json` and resolves the sync root + relative paths for the item Explorer is asking about.
   - Returns the correct overlay index (0 = highest priority tier you define) from `GetOverlayIconLocation` / `GetPriority` per `IShellIconOverlayIdentifier`.
2. Ship icons as **`.ico`** (multiple sizes) or paths to `.png` as required by your implementation.
3. Register the DLL with **regsvr32** or your installer (WiX, NSIS, etc.); Tauri’s bundle step can copy the DLL next to the executable and run registration **elevated** once.

The Rust crate does not register overlays automatically; this folder documents the contract only until a COM DLL is added to the repo.
