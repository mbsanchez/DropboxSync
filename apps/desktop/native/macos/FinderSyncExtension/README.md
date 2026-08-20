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

Open `DropboxSyncFinderSync.xcodeproj` to edit Swift. Embedding into the main app at **`Contents/PlugIns/`** is automatic via `bundle.macOS.files`, and host app and extension must use the **same Apple team**. `Info.plist.example` mirrors the committed `Info.plist` for reference.

**Do not set the Team under Signing & Capabilities.** The GUI writes `DEVELOPMENT_TEAM` into the committed `project.pbxproj`, which is exactly what the signing section below keeps empty on purpose. Export `APPLE_TEAM_ID` instead.

After install, the user may need to enable the extension under **System Settings → Privacy & Security → Extensions → Finder** (wording varies by macOS version).

### Replacing the appex during development

Swapping the `.appex` inside an installed `.app` and re-signing is **not** enough. Finder keeps using the
extension instance it already holds, so `init()` never re-runs, `directoryURLs` is never re-established, and
`requestBadgeIdentifier(for:)` is never called again — the badges simply stop appearing, with no error and no
log line. Restarting Finder alone does not clear it. Re-register instead:

```bash
pluginkit -e ignore -i com.mobsanchez.dropboxsyncdesktop.findersync
pluginkit -e use    -i com.mobsanchez.dropboxsyncdesktop.findersync
killall Finder
```

Note that `pluginkit -m -i <id> -vvv` keeps reporting the same UUID and timestamp across this cycle, so its
output cannot be used to tell whether the toggle took effect. Confirm from the extension side instead: with a
`Logger` call at the top of `requestBadgeIdentifier(for:)`, opening the sync folder should produce a burst of
queries within a second.

## ADR: App Sandbox on, Hardened Runtime on, no App Group

**Status:** Accepted — 2026-08-19 (DBSYNC-76). **Supersedes** the original "no App Sandbox" decision
taken the same day under DBSYNC-72, which was wrong and is recorded at the bottom of this section.

**Decision.** The extension **is sandboxed**. `DropboxSyncFinderSync.entitlements` declares
`com.apple.security.app-sandbox` together with a read-only temporary exception for
`/Library/Application Support/DropboxSyncDesktop/`, so it still reads `overlay_state.json` from the
host's own directory. Hardened Runtime stays on. No App Group is introduced and `db::app_data_dir()`
is unchanged.

**Context — why the earlier decision failed.** Without `com.apple.security.app-sandbox`, macOS silently
refuses to register the extension: it never appears in System Settings → Extensions → Finder, `pluginkit`
lists no entry for it, and Finder never loads it. There is no error anywhere — not in Console, not from
`codesign`, not from `pluginkit`. Every working Finder Sync extension on a reference Mac is sandboxed
(Dropbox's `garcon.appex`, OneDrive's `FinderSync.appex`, odrive's), and Apple's own Xcode template for the
target generates an entitlements file with the sandbox key. Adding the entitlement made registration work
immediately.

**Why no App Group.** Sandboxing normally forces the shared state file into
`~/Library/Group Containers/<group-id>/`, which would mean rewriting `db::app_data_dir()`, migrating every
existing user's state, registering the group with Apple, and shipping a provisioning profile. A read-only
`com.apple.security.temporary-exception.files.home-relative-path.read-only` entitlement avoids all four —
the same technique Dropbox uses for `~/.dropbox/`. The exception is read-only and scoped to one directory;
the extension never writes.

**The container trap.** Inside the sandbox, every Foundation home API — `homeDirectoryForCurrentUser`,
`NSHomeDirectory()`, and `.userDomainMask` searches — is redirected to
`~/Library/Containers/<bundle-id>/Data/`, whose `Library/Application Support` holds nothing but Apple's own
symlinks. The temporary exception is relative to the **real** home, so `overlayStateURL()` must build the
path from `getpwuid(getuid())`, which reads the passwd database and is not redirected. Resolving through
`FileManager` yields a path that simply does not exist: `Data(contentsOf:)` fails, `state` stays `nil`,
`directoryURLs` stays empty, and Finder never even asks for a badge — with no sandbox denial logged, because
the protected path was never touched. `reloadState()` logs that failure for exactly this reason.

**Entitlements are a property of the shipped artifact**, not of the sources, and the two can diverge:
`xcodebuild` injects `com.apple.security.get-task-allow` unless told not to, which is why `build-appex.sh`
disables that injection on every path, signed or ad-hoc. Check the product, never the project file:

```bash
codesign -d --entitlements - build/DropboxSyncFinderSync.appex
```

must print the sandbox key and the temporary exception, and **must not** print `get-task-allow` —
notarization refuses that outright. If it appears, the appex was built by something other than that script
(a plain `xcodebuild`, or Xcode.app) and must not be shipped.

**Trade-off accepted.** The temporary-exception entitlement is not accepted on the Mac App Store. Submitting
there would require reopening this decision and doing the App Group migration — but App Store distribution is
not a goal; the app ships with Developer ID.

---

**Superseded — original ADR (DBSYNC-72, 2026-08-19): "no App Sandbox".** It held that the extension should
carry no entitlements at all, on the grounds that App Sandbox is mandatory only for the App Store while
Developer ID needs Hardened Runtime plus notarization. That reasoning is correct about *distribution* and
irrelevant to *registration*, which is what actually broke. Its contingency clause said badges merely being
absent was "not the trigger" for reopening the ADR, and pointed at a stale `overlay_state.json` (DBSYNC-75) or
the decoder key-conversion issue (DBSYNC-73) as likelier causes; both were investigated and neither was the
cause. It also claimed the choice was "verified rather than assumed" because the resolution logic of
`overlayStateURL()` had been run outside a container — a test that could not fail, since it never exercised
the sandboxed case.

## Signing: environment-driven, never committed

Nothing in the repository names a real identity. Two committed values look like they might, and both are
deliberate fallbacks rather than pins:

- `tauri.conf.json` keeps `bundle.macOS.signingIdentity: "-"` (ad-hoc). **`APPLE_SIGNING_IDENTITY` wins over
  it** — tauri-cli reads the environment variable first and falls back to the config key only when it is unset
  (`interface/rust.rs`, verified in 2.10.1). The `"-"` must stay: `tauri-bundler` skips its entire signing
  block when no identity resolves, so removing it would leave unsigned builds with no signature and no
  `CodeResources` seal over `Contents/PlugIns/*.appex`.
- `project.pbxproj` keeps `DEVELOPMENT_TEAM = ""`. `build-appex.sh` passes the team to `xcodebuild` as a
  command-line override, which takes precedence, so a committed team id would buy nothing.

For a signed build, export both; the same team must be used for the app and the extension:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"   # host app, read by tauri build
export APPLE_TEAM_ID=TEAMID                                                    # appex build + notarization
npm run build:finder-sync    # reads APPLE_TEAM_ID only
npm run tauri build          # reads both
```

Three things about `build-appex.sh` that are easy to get wrong:

1. **A team alone is not enough — it breaks the build.** The project uses `CODE_SIGN_STYLE = Automatic`, and
   automatic signing resolves a *development* certificate for a build action. With a team set and no **Mac
   Development** certificate in the keychain — which a Developer ID-only account never has — xcodebuild fails
   with `No signing certificate "Mac Development" found`. The script therefore switches to
   `CODE_SIGN_STYLE=Manual` and names `CODE_SIGN_IDENTITY` explicitly whenever a team is present. Override it
   with `APPEX_CODE_SIGN_IDENTITY` if you need something other than `Developer ID Application`.
2. **`xcodebuild build` injects `com.apple.security.get-task-allow`** by default
   (`CODE_SIGN_INJECT_BASE_ENTITLEMENTS`). That is a debugging entitlement: notarization refuses it outright,
   and it defeats part of Hardened Runtime. The script sets it to `NO`.
3. **Xcode signs with `--timestamp=none`**, while notarization requires a secure TSA timestamp on every nested
   bundle. The script passes `OTHER_CODE_SIGN_FLAGS=--timestamp`.

Points 2 and 3 are not optional polish, because **this signature is final**: Tauri does not re-sign the appex.
`copy_custom_files_to_bundle()` (`tauri-bundler` 2.8.1, `bundle/macos/app.rs`) places it into
`Contents/PlugIns/` without adding it to the bundler's `sign_paths`, and the `codesign` invocation itself —
which lives in `tauri-macos-sign` 2.3.3, not in the bundler — carries no `--deep`. Whatever is wrong here
reaches Apple unchanged. Worse, once an identity resolves *and* notarization credentials are present, the
bundler submits automatically, so a bad signature goes to Apple without anyone asking for it.

With neither variable exported the appex is ad-hoc signed (`Signature=adhoc`, `TeamIdentifier=not set`), which
is correct for development and for CI, and cannot be notarized.

## Why not pure Tauri?

Finder badge overlays are implemented only via **App Extension** APIs (`FIFinderSync` / `FIFinderSyncController`). The Rust/Tauri process cannot draw those badges itself.
