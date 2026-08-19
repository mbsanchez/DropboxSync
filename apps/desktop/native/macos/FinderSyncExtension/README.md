# Finder Sync extension (macOS shell overlays)

The Tauri app writes **`overlay_state.json`** next to the SQLite database:

- Path: `~/Library/Application Support/DropboxSyncDesktop/overlay_state.json`
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

## ADR: no App Sandbox, Hardened Runtime on, no App Group

**Status:** Accepted — 2026-08-19 (DBSYNC-72).

**Decision.** The extension is **not sandboxed**. It ships with Hardened Runtime enabled and no entitlements
file, and it reads `overlay_state.json` straight from `~/Library/Application Support/DropboxSyncDesktop/`.
No App Group container is introduced and `db::app_data_dir()` is not changed.

**Context.** The app is distributed with **Developer ID** (direct download), not through the Mac App Store.
App Sandbox is mandatory only for the App Store; Developer ID requires Hardened Runtime plus notarization
instead, and both are already in place — `ENABLE_HARDENED_RUNTIME = YES` in both build configurations, and no
`CODE_SIGN_ENTITLEMENTS` key or `.entitlements` file exists anywhere under `apps/desktop`.

**Why it matters.** Sandboxing is the *only* reason this extension would need an App Group. A sandboxed
extension cannot read the host app's Application Support directory, which would force the state file into
`~/Library/Group Containers/<group-id>/`, force a rewrite of `db::app_data_dir()`, force a migration of every
existing user's state, and require a provisioning profile carrying the App Group entitlement. Staying
unsandboxed removes all four at no cost.

Verified rather than assumed: running the resolution logic of `overlayStateURL()` outside a container yields
the plain per-user directory, identical to what the Rust side writes.

**Trade-off accepted.** The app cannot be submitted to the Mac App Store without reopening this decision.

**Contingency — not a planned step.** If the extension cannot read the state file *and* Console.app shows a
sandbox or TCC denial for the appex, reopen this ADR and cost the App Group migration separately. Badges
merely being absent is **not** the trigger: likelier causes are a stale `overlay_state.json` (see DBSYNC-75)
or the decoder key-conversion issue (DBSYNC-73).

## Signing: environment-driven, never committed

`tauri.conf.json` deliberately does **not** set `bundle.macOS.signingIdentity`, and `project.pbxproj` keeps
`DEVELOPMENT_TEAM = ""`. A value in either place would take precedence over the environment and silently pin
an identity — which also breaks every unsigned build path, including CI, which holds no certificate.

Export both variables for a signed build; the same team must be used for the app and the extension:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"   # Tauri, host app
export APPLE_TEAM_ID=TEAMID                                                    # xcodebuild + notarization
npm run build:finder-sync
```

Two things about `build-appex.sh` that are easy to get wrong:

1. **A team alone is not enough.** The project uses `CODE_SIGN_STYLE = Automatic`, and automatic signing
   resolves a *development* certificate for a build action. With a team set and no **Mac Development**
   certificate in the keychain — which a Developer ID-only account never has — xcodebuild fails outright with
   `No signing certificate "Mac Development" found`. The script therefore switches to `CODE_SIGN_STYLE=Manual`
   and names `CODE_SIGN_IDENTITY` explicitly whenever a team is present. Override the identity with
   `APPEX_CODE_SIGN_IDENTITY` if you need something other than `Developer ID Application`.
2. **Xcode signs without a secure timestamp** (`--timestamp=none`). Notarization requires a secure timestamp on
   every nested bundle, so the `.appex` as produced here is *not* notarizable on its own: it must be re-signed
   with `--timestamp` before submission. Whether Tauri's bundling pass does that re-signing is still unverified
   — that is finding F4 of DBSYNC-72, resolved in Slice 4 (#84).

With neither variable exported the build is ad-hoc signed (`Signature=adhoc`, `TeamIdentifier=not set`), which
is correct for development and for CI, and cannot be notarized.

## Why not pure Tauri?

Finder badge overlays are implemented only via **App Extension** APIs (`FIFinderSync` / `FIFinderSyncController`). The Rust/Tauri process cannot draw those badges itself.
