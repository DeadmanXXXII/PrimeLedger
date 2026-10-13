# Builds a PrimeLedger .appxbundle from target\release\primeledger.exe.
# Run from the repo root (CI does). Needs the Windows SDK (makeappx, signtool).
# Tile images are pre-rendered in packaging\appx\Assets (mark on the square tiles,
# full lockup on the wide tile and splash) and are copied as-is.
#
#   Store build (unsigned, Partner Center signs on ingestion):
#     .\packaging\appx\build-appx.ps1 -Version 0.1.5.0 -Out dist\PrimeLedger-store.appxbundle
#
#   Sideload build (signed with your dev cert; Publisher is taken from the cert):
#     .\packaging\appx\build-appx.ps1 -Version 0.1.5.0 -Out dist\PrimeLedger-sideload.appxbundle `
#         -PfxPath cert.pfx -PfxPassword $env:CSC_KEY_PASSWORD

param(
    [string]$IdentityName = "TMHP.com.PrimeLedger",
    [string]$Publisher = "CN=BB6CF6A1-F89B-484C-85F4-1A7E35694B74",
    [string]$PublisherDisplayName = "@TMHP.com",
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$PfxPath,
    [string]$PfxPassword
)

$ErrorActionPreference = "Stop"

if ($Version -notmatch '^\d+\.\d+\.\d+\.0$') { throw "Version must be A.B.C.0 (Store requires revision 0), got '$Version'" }
if (-not (Test-Path target\release\primeledger.exe)) { throw "target\release\primeledger.exe not found - run cargo build --release -p prime-app first" }
if (-not (Test-Path packaging\appx\Assets\StoreLogo.png)) { throw "packaging\appx\Assets is missing the tile PNGs" }

# Sideload build: the manifest Publisher must equal the cert Subject exactly.
if ($PfxPath) {
    $cert = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new(
        (Resolve-Path $PfxPath).Path, $PfxPassword)
    $Publisher = $cert.Subject
    Write-Host "Sideload build, Publisher = $Publisher"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("appx-" + [guid]::NewGuid())
$stage = Join-Path $work "stage"
$bundleStage = Join-Path $work "bundle"
New-Item -ItemType Directory -Force -Path "$stage\Assets", $bundleStage | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path $Out -Parent) | Out-Null

Copy-Item target\release\primeledger.exe "$stage\primeledger.exe"

Copy-Item packaging\appx\Assets\*.png "$stage\Assets\"

function Esc($s) { [System.Security.SecurityElement]::Escape($s) }
(Get-Content packaging\appx\AppxManifest.xml.template -Raw) `
    -replace '__IDENTITY_NAME__', (Esc $IdentityName) `
    -replace '__PUBLISHER__', (Esc $Publisher) `
    -replace '__PUBLISHER_DISPLAY_NAME__', (Esc $PublisherDisplayName) `
    -replace '__VERSION__', $Version |
    Set-Content "$stage\AppxManifest.xml" -Encoding utf8

$sdkBin = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+\.\d+$' } | Sort-Object Name -Descending | Select-Object -First 1
$makeappx = Join-Path $sdkBin.FullName "x64\makeappx.exe"
$signtool = Join-Path $sdkBin.FullName "x64\signtool.exe"

& $makeappx pack /d $stage /p "$bundleStage\PrimeLedger-x64.appx" /o
if ($LASTEXITCODE -ne 0) { throw "makeappx pack failed" }
& $makeappx bundle /d $bundleStage /p $Out /o
if ($LASTEXITCODE -ne 0) { throw "makeappx bundle failed" }

if ($PfxPath) {
    & $signtool sign /fd SHA256 /f $PfxPath /p $PfxPassword $Out
    if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
}

Remove-Item $work -Recurse -Force
Write-Host "Built $Out"
