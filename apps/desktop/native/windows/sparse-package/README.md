# Sparse package (Windows package identity)

Grants the unpackaged DropboxSync exe a **package identity** via a **sparse
package** (an MSIX that contains only the manifest + logos; the exe stays external).
Identity is required for the WinRT **Cloud Files API** sync-root registration
(`StorageProviderSyncRootManager.Register`) that drives the Explorer **status
column** (epic DBSYNC-57) — without it that call throws `APPMODEL_ERROR_NO_PACKAGE`.

Per-user install, no admin **at runtime**; the one-time step that needs elevation is
**trusting the dev code-signing cert** (production ships a real cert / MSIX installer).

## Files
- `AppxManifest.xml` — the sparse-package manifest. `Publisher` MUST match the
  signing cert subject (`CN=DropboxSync Dev`); `AllowExternalContent` + the
  `unvirtualizedResources` capability make it an external-location package;
  `Executable` = `dropbox_sync_desktop.exe` (the dev exe under `target/release`).
- `build-and-install.ps1` — creates/reuses a self-signed cert, trusts it, packs
  (MakeAppx), signs (SignTool), and installs (`Add-AppxPackage -ExternalLocation`).

## Dev usage
```powershell
# 1) build the release exe (must exist at the external location)
npm --workspace apps/desktop run bundle:win

# 2) build + sign + install the sparse package (ELEVATED — trusts the dev cert)
powershell -ExecutionPolicy Bypass -File native\windows\sparse-package\build-and-install.ps1

# 3) launch WITH identity — `npm run dev:win` auto-detects the package and
#    launches via the app model. Verify: startup log shows package_identity=true.
npm --workspace apps/desktop run dev:win
```

## IMPORTANT: launch method grants identity, not the exe path
The exe gets package identity **only when launched through the app model** (the
package AUMID / Start-menu tile) — a **direct exe launch does NOT** get identity.
- Dev: `npm run dev:win` detects the installed package and launches via
  `explorer.exe shell:AppsFolder\<PackageFamilyName>!DropboxSyncDesktop`.
- Manual: `explorer.exe shell:AppsFolder\DropboxSyncDesktop_<hash>!DropboxSyncDesktop`
  (the build script prints the exact AUMID).
- **Auto-start on login** (when wired) must also go through the app model — use a
  packaged `windows.startupTask` extension in the manifest, or a launcher that
  invokes the AUMID — otherwise the auto-started instance has no identity and the
  CfAPI column silently disables.

## Uninstall
```powershell
Get-AppxPackage -Name DropboxSyncDesktop | Remove-AppxPackage
# Also remove the dev trust anchor (elevated) — it trusts anything signed by that
# self-signed key for 5 years; don't leave it lying around:
Get-ChildItem Cert:\LocalMachine\Root,Cert:\LocalMachine\TrustedPeople |
  Where-Object Subject -eq 'CN=DropboxSync Dev' | Remove-Item
```

## Production
Fold the manifest + assets into the eventual MSIX installer (real code-signing
cert), or ship the sparse package alongside the installed exe. The `Executable`
becomes the bundled name (`DropboxSyncDesktop.exe`) and the external location the
install dir.

## Notes
- `unvirtualizedResources` + `runFullTrust` capabilities are required for an
  external-location full-trust win32 app.
- The app detects identity at runtime (`src/windows_identity.rs`) and disables the
  CfAPI features cleanly when the sparse package isn't installed.
