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

- macOS: `~/Library/Application Support/DropboxSyncDesktop/overlay_state.json`
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

### Windows `.cloudsc` association (open / hydrate)

- `bundle.fileAssociations` in `src-tauri/tauri.conf.json` is applied when you build an **NSIS** or **MSI** installer (`tauri build` with those targets). A release `.exe` only (e.g. `npm run bundle:win` with `--no-bundle`) does **not** register file types; install the generated installer to get “Open with” / default app for `.cloudsc`.
- **Portable testing (no installer):** on each launch the app registers a **per-user** association under `HKCU\Software\Classes` pointing `.cloudsc` at the **current** executable (ProgID `DropboxSyncDesktop.CloudscPortable.1`). That lets double-click work without NSIS. Set `DROPBOXSYNC_SKIP_WINDOWS_FILE_ASSOC=1` to disable this. If Windows still opens another handler by default, use **Open with → Choose another app** once.
- On **Windows and Linux**, the shell passes the file path on the process command line (there is no `RunEvent::Opened` like on macOS/iOS). The app reads those args on startup and uses `tauri-plugin-single-instance` when a second launch occurs while the app is already running.

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

## macOS release architecture: arm64 only (DBSYNC-72 Slice 4)

**Decision (2026-08-20): the shipped macOS build is arm64-only.** Intel support is deferred,
not refused — see the trigger below.

Measured on the signed release bundle:

```
host  (Contents/MacOS/dropbox_sync_desktop)  -> arm64
appex (DropboxSyncFinderSync)                -> x86_64 arm64
```

The asymmetry is real and sits on the Rust/Tauri side. `build-appex.sh` builds the Finder Sync
extension universal because that is what `xcodebuild` does by default here; `tauri build`
produces a host binary for the build machine's architecture only.

**Why arm64-only.** GitHub's `macos-latest` runner is arm64, so the CI release workflow
(Slice 5, GitHub #85) reproduces this build with no extra configuration and no cross-target
toolchain. Shipping universal would mean passing `--target universal-apple-darwin` to
`tauri build`, which requires the x86_64 Rust target installed, roughly doubles the Rust link
step, and enlarges the bundle — for a user base this project does not yet have.

**Consequence to keep in mind.** The appex carries x86_64 slices the host can never load. That
is dead weight, not a bug: a universal appex inside an arm64 host is valid and notarizes
normally. Do not "fix" it by forcing the appex to arm64 unless the bundle size actually
matters — keeping it universal is what makes a later switch to a universal host cheap.

**When to revisit.** Ship universal if any of these becomes true: a real user reports an Intel
Mac; the project starts distributing publicly rather than to the maintainer; or the CI runner
image changes architecture. At that point the change is one flag on the Tauri build plus the
x86_64 Rust target — Slice 5's workflow must then request the target explicitly rather than
inheriting whatever the runner happens to be.
