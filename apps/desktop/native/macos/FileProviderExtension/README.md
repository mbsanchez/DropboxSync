# File Provider extension (DBSYNC-95)

`DropboxSyncFileProvider.appex` is an `NSFileProviderReplicatedExtension` (extension point
`com.apple.fileprovider-nonui`) written in Swift. It ships in every macOS build next to the Finder Sync appex
and does nothing until the host app registers a File Provider domain.

It lists the host's projection of the Dropbox tree, downloads a file when it is opened, and sends creates,
edits, renames, moves and deletes made in Finder to Dropbox through the host. The process model (the host is the only Dropbox client;
the extension has no network and no keychain access) is
[ADR-0004](../../../../docs/adr/0004-host-is-the-single-dropbox-client.md).

## Files

| File | Purpose |
| --- | --- |
| `FileProviderExtension.swift` | The extension, its items and the enumerator. Logs with `os.Logger`. |
| `ProjectionModel.swift`, `ProjectionStore.swift` | The read-only projection in `app.db`. |
| `Exchange.swift` | Asks the host for a file, or to write a change. |
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

There is no switch since the cutover (DBSYNC-98). After connecting Dropbox, onboarding shows *Put Dropbox in
Finder*; its *Start* adds the domain and shows the folder macOS gave it. Disconnecting Dropbox in Settings
removes it. Each launch makes the system match the breadcrumb, the `app_config` row
`file_provider_domain_registered`, and logs the decision (`File Provider startup decision`):

| Breadcrumb | Domain registered | Result |
| --- | --- | --- |
| present | any | the domain is added (idempotent) |
| absent | yes | the domain is removed, keeping unsynced changes |
| absent | no | nothing |

A Mac that used the sync folder forgets it at the first launch of this build; the folder stays on disk.

## The projection

Once Dropbox is in Finder, the host maintains `fp_identity`, `fp_item` and `fp_state` in `app.db` (the group
container). The extension reads them through `ProjectionStore.swift`, one read-only open and one read
transaction per call, and writes nothing; `ProjectionModel.swift` holds the decisions (ids, anchors, pages,
change batches) and imports Foundation only. `Tests/projection-schema.sql` is the DDL, kept identical to the
host's by a Rust test; `Tests/run-projection-tests.sh` runs the model and the store against it, in CI too.

After each commit the host asks the system to re-enumerate the working set. A delta that removed something
holds that signal for 10 s (60 s at most for a run of them), because Dropbox can deliver a folder rename as
all its deletes and, about 3 s later, all its re-adds (ADR-0004, *A rename split in two*). Before *Start*
nothing is written, and disconnecting empties the tables.

A failed read is logged by class and told to the system in one of two ways (`ReadFailure.answer`). SQLite busy
or failing an I/O is a transient error, which the system retries. Anything only the host can cure (a missing
or damaged file, unpopulated, newer schema, no container) is `serverUnreachable`, which shows an error and
backs off until the next signal. `currentSyncAnchor` answers `nil` for any failed read. `noSuchItem` is kept for an identifier with no row at all,
because the system takes it to mean the item was deleted and tries to delete it from disk; a live file asked
for as a container lists itself.

Opening a file downloads it (see *Download on open*), and changes made in Finder go to Dropbox (see
*Writes*).

How it is fed: the entries of each Dropbox delta are applied one at a time, in Dropbox's order, in one write
transaction, and each item changes only through its own entry (no inference about a renamed folder's
descendants; a snapshot places rows by id, sweeps what was not listed, then resolves parents again). The rules
come from Dropbox's classic `/delta` contract, are not in the current v2 docs we could read, and were checked
only against the shapes captured in #234. For the extension: a folder at state ORPHAN is not a parent, so a
live or hidden row never has an orphan parent, and an orphan (`parent_item = -1`) is never listed. Details in
the module docs of `src-tauri/src/storage/db/projection.rs` and in ADR-0004.

What feeding it costs (it is not free, and "no extra call" would be wrong):

- **A full listing at *Start*.** An unpopulated projection makes the stored delta cursor count as
  absent (it is not wiped), so the next loop iteration takes a snapshot (a recursive `list_folder` of the whole
  account); if that listing fails, the stored cursor is used and the reseed is retried later. The same happens after a Dropbox
  cursor reset, after a disconnect, and after the projection gives up on a failing delta
  (a listing at most every 30 minutes while it keeps failing, 1 minute at first, doubling).
- **The remote long-poll loop runs for the projection alone.** On macOS the loop that waits for Dropbox
  changes runs once Dropbox is in Finder, whatever `sync_folder` holds. Each wake-up applies the delta to the
  projection (one write transaction) and nothing else.
- **Storage:** one row per file and folder of the account, plus up to 50 000 tombstones.

To inspect it without launching anything, copy `app.db` and `app.db-wal` together and open the copy read-only
(the rows may be in the `-wal`, and a read-only open fails without it):
`sqlite3 'file:app-copy.db?mode=ro' 'select state, count(*) from fp_item group by state; select * from fp_state;'`.
The host logs counts only (`File Provider projection committed`, `working set signalled`), never names.

## Download on open

`fetchContents` asks the host through `fp-exchange/` beside `app.db`; `Exchange.swift` is this side and
`src-tauri/src/file_provider_exchange.rs` documents the protocol. To see one request:

```bash
/usr/bin/log show --last 5m --info --debug --predicate 'subsystem == "com.mobsanchez.dropboxsyncdesktop.fileprovider" AND eventMessage CONTAINS "fetchContents"'
```

Each line ends in the class of the outcome: `staged`, `hostDown`, `offline`, `auth`, `refused`, `retry` or `cancelled`. An
empty `requests/`, `staging/` and `results/` is the resting state; a download that was cancelled clears when it
finishes. The host's side logs under
`dropbox_sync_desktop_lib::file_provider_exchange`, and `=debug` shows each notification it receives.

## Writes

`createItem`, `modifyItem` and `deleteItem` go through the same exchange: new bytes are cloned into
`staging/<id>.up`, the host calls Dropbox and writes the row from Dropbox's answer. The rules (conflicted
copies, `deletionRejected`, one write at a time, upload sessions above one 8 MiB chunk, the mass-deletion breaker,
Move to Trash, `ItemFormat`, reimports) are in ADR-0004, *Writes (slice 4)*. To follow them:

```bash
/usr/bin/log show --last 5m --info --predicate 'subsystem == "com.mobsanchez.dropboxsyncdesktop.fileprovider" AND (eventMessage CONTAINS "Item")'
```

Each line ends in the outcome: `written`, `conflict`, `collision`, `deleted`, `deletionRejected`, `hostDown`,
`offline`, `auth`, `retry` or `cancelled`. The host logs under `dropbox_sync_desktop_lib::file_provider_writes`
by error class, never by name.

**A build that changes what an item reports** (its capabilities, above all) bumps `ItemFormat.current` in
`ProjectionModel.swift`. Without it the system keeps what it cached for every item it already knows.

## Offline (slice 5)

Two actions in `Info.plist`, **DropboxSync: Make Available Offline** and **DropboxSync: Make Online-Only**,
reach `performAction` (`NSFileProviderCustomAction`). Each item is one `pin` or `unpin` request; the host
records it in `fp_pin` and moves the row and what is under it in the change sequence, so the item comes back with
`contentPolicy = .downloadEagerlyAndKeepDownloaded` and a `p.` in its metadata version; what is under a
pinned folder comes back with `userInfo.inherited` and an `i.`, and is offered neither action. A pin also
names the domain's visible folder, and the host reads one byte of every live file at or under the item
there (two readers shared by every pin), which downloads it now: the policy alone only schedules a background
download, held while on battery. After each delta the host also reads the pinned files it changed. Unpinning
evicts. The system's Download Now and Remove Download are hidden. Rules and measurements are in ADR-0004,
*Offline (slice 5)*. The extension logs `pin <id>: <outcome>` and `unpin <id>: <outcome>`; the host logs
`File Provider pin: reading the pinned files` with the count.

## Live checks (with the maintainer)

1. **Strip quarantine after copying the app.** `ditto` keeps the quarantine attribute, and a quarantined app
   runs translocated from a randomised read-only path, where the extension is not found:
   `xattr -dr com.apple.quarantine /Applications/DropboxSyncDesktop.app`.
2. Switch on and relaunch (above).
3. The domain mounts: `ls ~/Library/CloudStorage/` shows a `DropboxSync...` folder containing the Dropbox
   root.
4. The extension is registered: `pluginkit -m -p com.apple.fileprovider-nonui -v` lists
   `com.mobsanchez.dropboxsyncdesktop.fileprovider`.
5. The enumerator is called. Use `/usr/bin/log`, not the shell builtin:
   `/usr/bin/log stream --predicate 'subsystem == "com.mobsanchez.dropboxsyncdesktop.fileprovider"'`
   shows `init`, `enumerator(for:)` and `enumerateItems rows=<n>`.
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

Disconnect Dropbox in Settings: the host clears the breadcrumb, then removes the domain. The removal asks the
system to preserve local changes that were never synced (`NSFileProviderDomainRemovalModePreserveDirtyUserData`)
and logs where they went; a removal that fails is retried at the next launch. Connect again and press *Start*
to add it again.

**Orphaned domain.** If `app.db` is lost while a domain is registered, the next launch finds no breadcrumb and
removes the domain; onboarding then adds it again.

`addDomain` for a replicated domain fails with `NSFileWriteFileExistsError` if the on-disk location already
exists. If re-adding fails after a removal, look for a leftover `DropboxSync...` folder under
`~/Library/CloudStorage/` (quit the app, and move it aside rather than deleting it) and relaunch. That
sequence is unverified; the first live run decides whether it is needed.
