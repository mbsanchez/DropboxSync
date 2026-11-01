# Finder Sync extension (macOS shell overlays)

The Tauri app writes **`overlay_state.json`** next to the SQLite database:

- Path: `~/Library/Group Containers/<TEAM_ID>.com.dropboxsyncdesktop/Library/Application Support/DropboxSyncDesktop/overlay_state.json` (the App Group container, see the ADR below; `app.db` and the logs live in the same directory)
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

## ADR: App Sandbox on, Hardened Runtime on, shared App Group

**Status:** Accepted — 2026-08-19 (DBSYNC-76), amended by DBSYNC-97. **Supersedes** the original "no App
Sandbox" decision taken the same day under DBSYNC-72, which was wrong and is recorded at the bottom of this
section. DBSYNC-76 also chose a read-only temporary exception over an App Group; DBSYNC-97 replaced that.

**Decision.** The extension **is sandboxed** and Hardened Runtime stays on. The host and the extension both
carry `com.apple.security.application-groups = [<TEAM_ID>.com.dropboxsyncdesktop]`. On macOS the host's whole
data directory (`app.db`, logs, `overlay_state.json`) is
`<group container>/Library/Application Support/DropboxSyncDesktop/`, where the group container is
`~/Library/Group Containers/<TEAM_ID>.com.dropboxsyncdesktop/`. The extension reads `overlay_state.json` from
there; the container is outside both sandboxes, so neither needs a temporary exception.

**Context — why the earlier decision failed.** Without `com.apple.security.app-sandbox`, macOS silently
refuses to register the extension: it never appears in System Settings → Extensions → Finder, `pluginkit`
lists no entry for it, and Finder never loads it. There is no error anywhere — not in Console, not from
`codesign`, not from `pluginkit`. Every working Finder Sync extension on a reference Mac is sandboxed
(Dropbox's `garcon.appex`, OneDrive's `FinderSync.appex`, odrive's), and Apple's own Xcode template for the
target generates an entitlements file with the sandbox key. Adding the entitlement made registration work
immediately.

**Why an App Group, and why it is team-prefixed.** A sandboxed extension cannot read the host's real
`~/Library/Application Support`. The first fix (DBSYNC-76) was a read-only
`temporary-exception.files.home-relative-path.read-only` entitlement plus `getpwuid(getuid())` to bypass the
container redirection of Foundation's home APIs. It worked, but the exception is not accepted on the Mac App
Store and it left the extension depending on a path outside its sandbox. A group id that starts with the
signing team (`<TEAM_ID>.`) has needed no provisioning profile and no registration with Apple so far: Dropbox
ships a notarized appex that way, and `codesign` shows the entitlement on both of our binaries with no profile.
This is what has been observed, not a guarantee: notarization acceptance and the container materializing at
runtime are not yet verified (DBSYNC-97 Slice 3, #229). The group is shared, not a copy: `db::app_data_dir()` resolves into
the container, and `getpwuid` and `realHomeDirectory` are gone.

**The team ID is no longer kept in the tree; older commits contain it.** A team ID is not a secret: it is stamped
into every signed binary. The literal one was removed from this public repository as a maintainer's choice (hygiene,
and so that forks build with their own team); history is not rewritten. The entitlements files in git hold the token `__APP_GROUP_ID__`;
`scripts/render-entitlements.mjs` substitutes `<APPLE_TEAM_ID>.com.dropboxsyncdesktop` into gitignored copies at
build time (the host's goes through `tauri-before-build.mjs`, the extension's through `build-appex.sh`). With no
`APPLE_TEAM_ID`, or with `APPLE_SIGNING_IDENTITY` unset, empty or `-` (ad-hoc), the whole `application-groups` key is
dropped rather than signed bare. The same renderer serves both, so the host and the appex always agree.

**A working macOS build needs `APPLE_TEAM_ID` and a real `APPLE_SIGNING_IDENTITY`.** Only a build with both gets the
group: the renderer drops the `application-groups` key when the identity is unset, empty or `-`, for the host and the
appex alike (`tauri.conf.json` keeps `signingIdentity: "-"`, so an ad-hoc `dev:mac` or `bundle:dev` with only
`APPLE_TEAM_ID` exported renders no group and warns). Foundation returns a container URL even for a
group the process is not entitled to (and using it can raise the "access data from other apps" prompt), so the
host does not trust that URL: `app_group::container_path()` first reads the process's own signed
`com.apple.security.application-groups` entitlement and returns `None` unless it contains the build's group. A host
without the group (every ad-hoc or team-less `bundle:dev`) therefore never touches the container and fails at startup
with an error naming `APPLE_TEAM_ID`, because it never falls back to a directory the extension cannot read. What a
bare `tauri dev` run does is unverified until #229, as is the whole runtime behaviour; at startup the app logs
which groups its signature carries and whether the expected one was found ("App Group entitlement check"). The extension has no such pre-check: an unentitled appex still gets a URL
from `containerURL(forSecurityApplicationGroupIdentifier:)` and its first read is denied by the sandbox, which
`reloadState()` logs. Export `APPLE_TEAM_ID` and `APPLE_SIGNING_IDENTITY` (see *Signing* below), or put
`APPLE_TEAM_ID` in the gitignored `apps/desktop/.env`. The release workflow refuses an invalid team before building,
and, between build and notarization, runs `scripts/check-app-groups.mjs`: the host and every nested appex must
carry exactly `<TeamIdentifier>.com.dropboxsyncdesktop`, and each appex `Info.plist` `DBSyncAppGroupID` must equal
the group in its own entitlements.

**The entitlements path is content-addressed.** `build-appex.sh` renders the entitlements and copies them to
`build/entitlements-<first 16 hex of sha256>/DropboxSyncFinderSync.entitlements`. `xcodebuild` records the
entitlements file's modification time and fails the next build with "Entitlements file ... was modified
during the build" if the same path is rewritten (for example when the team changes). A new content yields a
new path, and an unchanged one is never rewritten.

**Entitlements are a property of the shipped artifact**, not of the sources, and the two can diverge:
`xcodebuild` injects `com.apple.security.get-task-allow` unless told not to, which is why `build-appex.sh`
disables that injection on every path, signed or ad-hoc. Check the product, never the project file:

```bash
codesign -d --entitlements - build/DropboxSyncFinderSync.appex
```

must print the sandbox key and the application group, and **must not** print `get-task-allow` —
notarization refuses that outright. If it appears, the appex was built by something other than that script
(a plain `xcodebuild`, or Xcode.app) and must not be shipped.

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
