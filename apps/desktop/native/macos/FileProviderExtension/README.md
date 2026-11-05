# File Provider extension (DBSYNC-95)

`DropboxSyncFileProvider.appex` is an `NSFileProviderReplicatedExtension` (extension point
`com.apple.fileprovider-nonui`) written in Swift. It ships in every macOS build next to the Finder Sync appex
and does nothing until the host app registers a File Provider domain.

**Slice 1 is a tracer.** The enumerator serves one static folder, `File Provider tracer`; create, modify,
delete and download answer with `cannotSynchronize`. It exists to prove that the extension loads, the domain
mounts under `~/Library/CloudStorage`, and notarization passes with two appexes. The process model (the host
is the only Dropbox client; the extension has no network and no keychain access) is
[ADR-0004](../../../../docs/adr/0004-host-is-the-single-dropbox-client.md).

## Files

| File | Purpose |
| --- | --- |
| `FileProviderExtension.swift` | The extension, the static item and the enumerator. Logs with `os.Logger`. |
| `Info.plist` | Extension point, bare ObjC principal class, `NSExtensionFileProviderSupportsEnumeration`, `DBSyncAppGroupID`. Deliberately no `NSExtensionFileProviderDocumentGroup`. |
| `DropboxSyncFileProvider.entitlements` | Template: sandbox plus `__APP_GROUP_ID__`. No network entitlement. |
| `build-appex.sh` | Builds and signs the appex. Copied from the Finder Sync script. |
| `DropboxSyncFileProvider.xcodeproj` | Deployment target 13.0, hardened runtime, `DEVELOPMENT_TEAM` empty. |

## Build

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: <Name> (<TEAM_ID>)"
export APPLE_TEAM_ID=<TEAM_ID>
npm --workspace apps/desktop run build:file-provider
```

Output: `build/DropboxSyncFileProvider.appex` (gitignored). `npm run bundle:dev` and the release workflow run
this through `scripts/tauri-before-build.mjs`, and Tauri copies the appex into `Contents/PlugIns/` through
`bundle.macOS.files`.

**Signing is the same as the Finder Sync appex** (see its README, *Signing*): the entitlements are rendered
from the template by `scripts/render-entitlements.mjs` into `build/`, with the group
`<TEAM_ID>.com.dropboxsyncdesktop` taken from `APPLE_TEAM_ID`, signed manually with `--timestamp`, with no
`get-task-allow`. The signature is final: Tauri does not re-sign the appex. The team id is never a literal in
the tree. `scripts/appex-scripts.test.mjs` fails if the two build scripts drift apart on those flags.

Check the result:

```bash
codesign -d --entitlements - --xml build/DropboxSyncFileProvider.appex   # sandbox + the group, no network
plutil -p build/DropboxSyncFileProvider.appex/Contents/Info.plist        # NSExtensionPointIdentifier
```

## Turning the domain on

The switch is a per-user defaults key, **off by default**. Nothing in the repository or the release
workflow writes it.

```bash
defaults write com.mobsanchez.dropboxsyncdesktop FileProviderEnabled -bool YES   # on
defaults delete com.mobsanchez.dropboxsyncdesktop FileProviderEnabled            # off again
```

The host reads it once at startup, so quit and relaunch the app after changing it. Only an explicit boolean
`true` turns it on. At startup the app logs the decision (`File Provider startup decision`):

| Switch | Breadcrumb | Result |
| --- | --- | --- |
| off | none | no File Provider call at all (today's behaviour) |
| off | present | the domain is removed and the breadcrumb cleared |
| on | any | the breadcrumb is set and the domain is added (idempotent) |

The breadcrumb is the `app_config` row `file_provider_domain_registered`.

## The projection (slice 2a, host side only)

With the switch on, the host also maintains `fp_identity`, `fp_item` and `fp_state` in `app.db` (the group
container). The extension does not read them yet (slice 2b); `Tests/projection-schema.sql` is the DDL it will
read, kept identical to the host's by a Rust test. After each commit the host asks the system to
re-enumerate the working set. With the switch off nothing is written, and turning it off empties the tables.

How it is fed: the entries of each Dropbox delta are applied one at a time, in Dropbox's order, in one write
transaction, and each item changes only through its own entry (no inference about a renamed folder's
descendants; a snapshot places rows by id, sweeps what was not listed, then resolves parents again). The rules
come from Dropbox's classic `/delta` contract, are not in the current v2 docs we could read, and were checked
only against the shapes captured in #234. For the extension: a folder at state ORPHAN is not a parent, so a
live or hidden row never has an orphan parent, and an orphan (`parent_item = -1`) is never listed. Details in
the module docs of `src-tauri/src/storage/db/projection.rs` and in ADR-0004.

**Open for 2b:** rename a folder of about 200 files on dropbox.com and capture whether every descendant is
re-listed.

What feeding it costs (it is not free, and "no extra call" would be wrong):

- **A full listing when the switch goes on.** An unpopulated projection makes the stored delta cursor count as
  absent (it is not wiped), so the next loop iteration takes a snapshot (a recursive `list_folder` of the whole
  account); if that listing fails, the stored cursor is used and the reseed is retried later. The same happens after a Dropbox
  cursor reset, after logout or a new sync folder, and after the projection gives up on a failing delta
  (a listing at most every 30 minutes while it keeps failing, 1 minute at first, doubling).
- **The breaker-gated reconcile on machines that have a sync folder.** The snapshot goes through the same
  seed as always, so a machine with a sync folder also runs the local-index reconcile (and its mass-delete
  breaker) at that point. A machine without one skips it.
- **The remote long-poll loop runs without a sync folder.** On macOS with the switch on, the loop that
  waits for Dropbox changes runs for the projection alone, where it used to idle until a folder was set.
  Each wake-up applies the delta to the projection (one write transaction) and nothing else.
- **Storage:** one row per file and folder of the account, plus up to 50 000 tombstones.

To inspect it without launching anything, open a COPY of `app.db` read-only:
`sqlite3 'file:app-copy.db?mode=ro' 'select state, count(*) from fp_item group by state; select * from fp_state;'`.
The host logs counts only (`File Provider projection committed`, `working set signalled`), never names.

## Live checks (with the maintainer)

1. **Strip quarantine after copying the app.** `ditto` keeps the quarantine attribute, and a quarantined app
   runs translocated from a randomised read-only path, where the extension is not found:
   `xattr -dr com.apple.quarantine /Applications/DropboxSyncDesktop.app`.
2. Switch on and relaunch (above).
3. The domain mounts: `ls ~/Library/CloudStorage/` shows a `DropboxSync...` folder containing
   `File Provider tracer`.
4. The extension is registered: `pluginkit -m -p com.apple.fileprovider-nonui -v` lists
   `com.mobsanchez.dropboxsyncdesktop.fileprovider`.
5. The enumerator is called. Use `/usr/bin/log`, not the shell builtin:
   `/usr/bin/log stream --predicate 'subsystem == "com.mobsanchez.dropboxsyncdesktop.fileprovider"'`
   shows `init`, `enumerator(for:)` and `enumerateItems`.
6. No permission dialog appears at any point.
7. Finder Sync badges still work outside the domain.

## If the domain does not mount

Likely causes, most probable first:

- Entitlements: the appex is sandboxed with the App Group and nothing else. The installed Dropbox File
  Provider appex also carries `com.apple.security.application-identifier`, which ours deliberately does not.
  Treat that as a suspect if the domain is registered but never mounts; do not add it before the other
  causes are ruled out.
- The app is translocated (step 1 above).
- `NSExtensionPrincipalClass` does not resolve (`build-appex.sh` guards this).
- A leftover on-disk location from an earlier domain (see below).

## Removing, then re-adding the domain

Switch off and relaunch: the host removes the domain and clears the breadcrumb. The removal asks the system to
preserve local changes that were never synced (`NSFileProviderDomainRemovalModePreserveDirtyUserData`) and logs
where they went. Then switch on and relaunch to add it again.

**Orphaned domain.** The breadcrumb lives in `app.db`. If that file is lost while a domain is registered, the
switch-off path no longer knows to remove it. Clean up by hand with the system tool `fileproviderctl` (its
usage is unverified here), or from System Settings, then relaunch.

`addDomain` for a replicated domain fails with `NSFileWriteFileExistsError` if the on-disk location already
exists. If re-adding fails after a removal, look for a leftover `DropboxSync...` folder under
`~/Library/CloudStorage/` (quit the app, and move it aside rather than deleting it) and relaunch. That
sequence is unverified; the first live run decides whether it is needed.

## Known limits of the tracer

- Not verified on a live system: the domain mounting, `NSFileProviderManager` calls through `objc2` and
  `block2` (a failure there is a crash, not a compile error), and notarization with two appexes.
- The host removes a domain it finds by identifier using a fresh `NSFileProviderDomain` object with the same
  identifier; that this matches is assumed from the header, not observed.
