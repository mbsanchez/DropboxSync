# Dropbox Sync Desktop

Desktop client built with Tauri + React + Rust for Dropbox synchronization, with:

- OAuth2 (PKCE + refresh token persistence in keychain)
- Local index + remote index + queued sync jobs
- Tray-first UX
- `.cloudsc` placeholder support
- Native shell overlay scaffolding:
  - macOS Finder Sync extension (`.appex`)
  - Windows Explorer overlay contract docs

## Project structure

- `src/`: React UI (Vite)
- `src-tauri/src/`: Rust backend (sync engine, Dropbox API, storage, commands)
- `src-tauri/src/overlay_state.rs`: computes `overlay_state.json` for native overlays
- `native/macos/FinderSyncExtension/`: Finder Sync extension project + build script
- `native/windows/shell-overlay/`: Windows overlay implementation notes
- `scripts/tauri-before-build.mjs`: pre-bundle hook for frontend + Finder extension

## Requirements

- Node.js 18+
- Rust toolchain
- On macOS:
  - Xcode Command Line Tools for standard Rust/Tauri builds
  - Full Xcode for Finder Sync extension (`.appex`) builds

## Install

From repo root:

```bash
npm install
```

Or from this workspace:

```bash
cd apps/desktop
npm install
```

## Environment variables (`.env`)

Create `apps/desktop/.env` from the example:

```bash
cp .env.example .env
```

Required:

- `DROPBOX_APP_KEY`

How to create `DROPBOX_APP_KEY`:

1. Open [Dropbox App Console](https://www.dropbox.com/developers/apps) and click **Create app**.
2. Choose **Scoped access** and **Full Dropbox** (or **App folder**, depending on your product scope).
3. Name the app and create it.
4. In **Settings**, copy the **App key** value.
5. Paste it into `apps/desktop/.env` as `DROPBOX_APP_KEY=...`.
6. In the same app settings, add this exact OAuth redirect URI:
   - `http://localhost:53682/callback`
7. Ensure required scopes are enabled (minimum: account info + files metadata/content according to your sync features), then save.

Optional (defaults to localhost callback):

- `DROPBOX_REDIRECT_URI=http://localhost:53682/callback`

Notes:

- `npm run tauri` and `npm run bundle:dev` automatically load `.env`.
- Values are embedded at compile time for the Rust binary (`option_env!`), so after changing `.env`, rebuild the app.

## Development

From repo root:

```bash
npm --workspace apps/desktop run tauri dev
```

Or from `apps/desktop`:

```bash
npm run tauri dev
```

## Build

### App bundle (desktop)

From repo root:

```bash
npm --workspace apps/desktop run bundle:dev
```

From `apps/desktop`:

```bash
npm run bundle:dev
```

`beforeBuildCommand` runs `npm run tauri:before-build`, which does:

1. `npm run build` (frontend)
2. on macOS only: `npm run build:finder-sync`

### Finder Sync extension only (macOS)

```bash
npm run build:finder-sync
```

Output:

- `native/macos/FinderSyncExtension/build/DropboxSyncFinderSync.appex`

Optional:

```bash
CONFIGURATION=Debug npm run build:finder-sync
APPLE_TEAM_ID=XXXXXXXXXX npm run build:finder-sync
```

## Native overlay integration

### Shared overlay state contract

The Rust backend writes:

- macOS: `~/Library/Applications/DropboxSyncDesktop/overlay_state.json`
- Windows: `%LOCALAPPDATA%\\DropboxSyncDesktop\\overlay_state.json`

Schema summary:

- `version`
- `updated_at`
- `sync_folder`
- `paths: { "<relative_path>": "synced|out_of_sync|syncing" }`

### macOS Finder

- `bundle.macOS.files` in `src-tauri/tauri.conf.json` copies the built `.appex` into:
  - `DropboxSyncDesktop.app/Contents/PlugIns/DropboxSyncFinderSync.appex`

### Windows Explorer

- See `native/windows/shell-overlay/README.md`
- COM overlay DLL implementation and installer registration are still pending

## Authentication behavior

- Full token session (`access_token`, `refresh_token`, `expires_at`) is persisted in keychain.
- Access tokens are refreshed automatically when near expiry.
- UI should only force login on hard auth failures (for example invalid/expired refresh grant), not transient network/service errors.

## Troubleshooting

- **`beforeBuildCommand` fails on Finder build**:
  - Ensure full Xcode is installed and selected:
    - `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`
- **Finder extension compiles but no badges**:
  - Confirm extension is enabled in System Settings > Extensions > Finder
  - Verify `overlay_state.json` exists and updates
- **macOS signing issues**:
  - Ad-hoc signing is not enough for production-like extension behavior
  - Use a valid signing identity and consistent team for app + extension

## Helpful scripts (`apps/desktop/package.json`)

- `npm run dev`
- `npm run build`
- `npm run tauri`
- `npm run bundle:dev`
- `npm run dev:mac` (macOS: build app bundle and open it)
- `npm run bundle:win` / `npm run dev:win` (Windows: release `.exe` only, no NSIS/MSI installer; `dev:win` also launches it)
- `npm run tauri:before-build`
- `npm run build:finder-sync`
