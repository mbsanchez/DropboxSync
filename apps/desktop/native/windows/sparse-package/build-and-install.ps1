<#
  DBSYNC-58: build, sign and install the sparse (external-location) package that
  grants the DropboxSync exe a package identity (needed for the WinRT Cloud Files
  API sync-root registration, DBSYNC-57).

  Run ELEVATED (it trusts the dev code-signing cert in LocalMachine). Per-user
  install. Uninstall with:  Get-AppxPackage -Name DropboxSyncDesktop | Remove-AppxPackage
#>
param(
  [string]$ExeDir  = (Join-Path $PSScriptRoot '..\..\..\src-tauri\target\release'),
  [string]$OutMsix = (Join-Path $env:TEMP 'DropboxSyncSparse.msix')
)
$ErrorActionPreference = 'Stop'

function Require-Admin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  if (-not ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script ELEVATED (it trusts the dev cert in LocalMachine).'
  }
}
Require-Admin

$ExeDir = (Resolve-Path $ExeDir).Path
$exe = Join-Path $ExeDir 'dropbox_sync_desktop.exe'
if (-not (Test-Path $exe)) { throw "exe not found: $exe  (build it first: npm run bundle:win)" }

# Derive the signing subject from the manifest <Identity Publisher> so the cert
# and the package publisher always match (Add-AppxPackage fails cryptically on a
# publisher/signature mismatch).
$manifestPath = Join-Path $PSScriptRoot 'AppxManifest.xml'
$CertSubject = ([xml](Get-Content -Raw $manifestPath)).Package.Identity.Publisher
Write-Host "Signing subject (from manifest Publisher): $CertSubject"

# 1. Self-signed code-signing cert (reused across runs). Subject MUST match the
#    manifest <Identity Publisher>.
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $CertSubject } | Select-Object -First 1
if (-not $cert) {
  Write-Host "Creating self-signed code-signing cert: $CertSubject"
  $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $CertSubject `
            -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
            -FriendlyName 'DropboxSync Dev Signing' -NotAfter (Get-Date).AddYears(5)
}
$thumb = $cert.Thumbprint

# 2. Trust it (machine-wide, so AppX deployment accepts the signature).
$cer = Join-Path $env:TEMP 'DropboxSyncDev.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\Root        | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null

# 3. Stage manifest + logos (reused from the app's MSIX icons).
$stage = Join-Path $env:TEMP 'DropboxSyncSparseStage'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path (Join-Path $stage 'Assets') -Force | Out-Null
Copy-Item (Join-Path $PSScriptRoot 'AppxManifest.xml') (Join-Path $stage 'AppxManifest.xml')
$icons = Join-Path $PSScriptRoot '..\..\..\src-tauri\icons'
foreach ($a in 'StoreLogo.png','Square150x150Logo.png','Square44x44Logo.png') {
  Copy-Item (Join-Path $icons $a) (Join-Path $stage "Assets\$a")
}

# 4. Locate the SDK tools.
$kit = 'C:\Program Files (x86)\Windows Kits\10\bin'
$makeappx = (Get-ChildItem $kit -Recurse -Filter makeappx.exe | Where-Object FullName -like '*x64*' | Sort-Object FullName -Descending | Select-Object -First 1).FullName
$signtool = (Get-ChildItem $kit -Recurse -Filter signtool.exe | Where-Object FullName -like '*x64*' | Sort-Object FullName -Descending | Select-Object -First 1).FullName
if (-not $makeappx) { throw 'MakeAppx.exe not found - install the Windows 10/11 SDK.' }
if (-not $signtool) { throw 'SignTool.exe not found - install the Windows 10/11 SDK.' }

# 5. Pack the sparse package (no exe inside - it lives at the external location).
Remove-Item $OutMsix -Force -ErrorAction SilentlyContinue
& $makeappx pack /d $stage /p $OutMsix /nv /o
if ($LASTEXITCODE -ne 0) { throw 'makeappx pack failed' }

# 6. Sign with the trusted dev cert.
& $signtool sign /fd SHA256 /sha1 $thumb $OutMsix
if ($LASTEXITCODE -ne 0) { throw 'signtool sign failed' }

# 7. Install with the external location = the exe's folder.
Get-AppxPackage -Name DropboxSyncDesktop | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Path $OutMsix -ExternalLocation $ExeDir

# 8. Verify.
$pkg = Get-AppxPackage -Name DropboxSyncDesktop
if (-not $pkg) { throw 'install verification failed (no package registered)' }
$aumid = "$($pkg.PackageFamilyName)!DropboxSyncDesktop"
Write-Host ""
Write-Host "Installed sparse package: $($pkg.PackageFullName)"
Write-Host "External location:        $ExeDir"
Write-Host "AUMID:                    $aumid"
Write-Host ""
Write-Host "IMPORTANT: the exe gets package identity ONLY when launched via the app"
Write-Host "model (a direct exe launch does NOT). Use 'npm run dev:win' (auto-detects"
Write-Host "the package), the Start-menu tile, or:"
Write-Host "  explorer.exe shell:AppsFolder\$aumid"
