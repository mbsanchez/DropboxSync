# enable-status-column.ps1  (run elevated)
#
# DBSYNC-41: enable Explorer's built-in cloud "Status" column for the DropboxSync
# sync root.
#
# Why this is needed: the WinRT StorageProviderSyncRootManager.Register API (which
# the app uses to register the sync root without elevation) always creates a
# navigation-pane node (NamespaceCLSID + a shell-folder CLSID under HKCU). That node
# makes the shell treat the sync folder as a namespace-extension root and STOPS it
# from rendering the per-file status column on the physical path. Removing the node
# (leaving the rest of the registration intact) restores the column with correct
# per-file badges.
#
# Run this once after the app has registered the sync root (dev: elevated shell;
# production: the installer performs the same steps).

[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'

$srmBase = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager'
$roots = Get-ChildItem $srmBase -ErrorAction SilentlyContinue |
    Where-Object { $_.PSChildName -like 'DropboxSync!*' }

if (-not $roots) {
    Write-Warning "No DropboxSync sync root found under SyncRootManager. Run the app first so it registers the sync root, then re-run this script."
    exit 1
}

$backupDir = Join-Path $env:ProgramData 'DropboxSync\syncroot-backup'
New-Item -ItemType Directory -Force -Path $backupDir | Out-Null

foreach ($r in $roots) {
    $id = $r.PSChildName
    $key = $r.PSPath
    $props = Get-ItemProperty $key
    $clsid = $props.NamespaceCLSID

    # Backup this sync-root entry + its CLSID before touching anything.
    $safe = ($id -replace '[!\\/:*?"<>|]', '_')
    reg export ("HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\$id") (Join-Path $backupDir "$safe.reg") /y | Out-Null

    Write-Host "Sync root: $id"
    Write-Host "  NamespaceCLSID = $clsid"

    # 1) Drop the navigation-pane node fields from the HKLM registration.
    foreach ($v in 'NamespaceCLSID', 'AUMID') {
        if ($null -ne $props.$v) {
            Remove-ItemProperty -LiteralPath $key -Name $v -ErrorAction SilentlyContinue
            Write-Host "  removed HKLM value: $v"
        }
    }

    # 2) Remove the shell-folder CLSID registration (HKCU, 64- and 32-bit views).
    if ($clsid) {
        $view = 0
        foreach ($base in 'HKCU:\Software\Classes\CLSID', 'HKCU:\Software\Classes\WOW6432Node\CLSID') {
            $p = Join-Path $base $clsid
            if (Test-Path $p) {
                reg export ($p -replace '^HKCU:', 'HKCU') (Join-Path $backupDir "clsid_${safe}_$view.reg") /y 2>$null | Out-Null
                Remove-Item -LiteralPath $p -Recurse -Force
                Write-Host "  removed CLSID: $p"
            }
            $view++
        }
    }
}

Write-Host ""
Write-Host "Restarting Explorer..."
Stop-Process -Name explorer -Force
Start-Sleep 1
Start-Process explorer

Write-Host "Done. The 'Status/Statut' column should now auto-appear in the sync folder."
Write-Host "Backups: $backupDir"
