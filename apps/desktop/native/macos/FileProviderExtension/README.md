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
