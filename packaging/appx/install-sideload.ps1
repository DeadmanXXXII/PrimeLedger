# Run LOCALLY as Administrator on a Windows test machine. Trusts your dev cert
# and installs the signed sideload bundle so you can launch it before submitting.
#   .\install-sideload.ps1 -Bundle .\PrimeLedger-sideload.appxbundle -PfxPath .\cert.pfx -PfxPassword '...'
param(
    [Parameter(Mandatory = $true)][string]$Bundle,
    [Parameter(Mandatory = $true)][string]$PfxPath,
    [Parameter(Mandatory = $true)][string]$PfxPassword
)
$ErrorActionPreference = "Stop"
$pw = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
$imported = Import-PfxCertificate -FilePath $PfxPath -CertStoreLocation Cert:\LocalMachine\TrustedPeople -Password $pw
Add-AppxPackage -Path $Bundle
Write-Host "Installed. Launch PrimeLedger from the Start menu."
Write-Host "Remove trust afterwards: Remove-Item Cert:\LocalMachine\TrustedPeople\$($imported.Thumbprint)"
