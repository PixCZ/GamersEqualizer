<#
.SYNOPSIS
  Generates a self-signed Code Signing certificate for GamersEqualizer.

.DESCRIPTION
  Creates a self-signed Authenticode certificate, exports:
  1. Private key + certificate (.pfx) for signing builds (stored in secrets\ by default, git-ignored).
  2. Public certificate (.cer) for testers to install (stored in packaging\ by default).

  Testers who install the public .cer into their "Trusted Root Certification Authorities"
  will not see SmartScreen warnings or Smart App Control blocks for binaries signed with this cert.

.PARAMETER Subject
  Certificate subject name. Default: "CN=GamersEqualizer Dev"

.PARAMETER PfxPath
  Path for the private .pfx file. Default: "secrets\GamersEqualizer-dev.pfx"

.PARAMETER CerPath
  Path for the public .cer file. Default: "packaging\GamersEqualizer-dev.cer"

.PARAMETER Password
  Password to protect the .pfx file. Default: "GamersEqualizer"

.PARAMETER ValidYears
  Validity period in years. Default: 5

.PARAMETER InstallLocal
  Also installs the certificate into the local machine's Trusted Root store (requires admin).

.EXAMPLE
  .\scripts\create-dev-cert.ps1
  .\scripts\create-dev-cert.ps1 -InstallLocal
#>
param(
  [string]$Subject = 'CN=GamersEqualizer Dev',
  [string]$PfxPath = 'secrets\GamersEqualizer-dev.pfx',
  [string]$CerPath = 'packaging\GamersEqualizer-dev.cer',
  [string]$Password = 'GamersEqualizer',
  [int]$ValidYears = 5,
  [switch]$InstallLocal
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $root

Write-Host "Creating self-signed code signing certificate: $Subject..." -ForegroundColor Cyan

# Ensure output directories exist
$pfxFull = if ([IO.Path]::IsPathRooted($PfxPath)) { $PfxPath } else { [IO.Path]::GetFullPath((Join-Path $root $PfxPath)) }
$cerFull = if ([IO.Path]::IsPathRooted($CerPath)) { $CerPath } else { [IO.Path]::GetFullPath((Join-Path $root $CerPath)) }
$pfxDir = [IO.Path]::GetDirectoryName($pfxFull)
$cerDir = [IO.Path]::GetDirectoryName($cerFull)
if (-not (Test-Path $pfxDir)) { New-Item -ItemType Directory -Force $pfxDir | Out-Null }
if (-not (Test-Path $cerDir)) { New-Item -ItemType Directory -Force $cerDir | Out-Null }

# Generate the certificate in CurrentUser\My
$notAfter = (Get-Date).AddYears($ValidYears)
$cert = New-SelfSignedCertificate `
  -Type CodeSigningCert `
  -Subject $Subject `
  -KeyUsage DigitalSignature `
  -FriendlyName 'GamersEqualizer Code Signing' `
  -NotAfter $notAfter `
  -CertStoreLocation 'Cert:\CurrentUser\My'

if (-not $cert) { throw 'Failed to create self-signed certificate.' }

Write-Host "Certificate generated:" -ForegroundColor Green
Write-Host "  Thumbprint : $($cert.Thumbprint)"
Write-Host "  Subject    : $($cert.Subject)"
Write-Host "  Expires    : $($cert.NotAfter)"

# Export to PFX (private key + cert)
$securePass = ConvertTo-SecureString -String $Password -Force -AsPlainText
Export-PfxCertificate -Cert $cert -FilePath $pfxFull -Password $securePass | Out-Null
Write-Host "Exported private key: $pfxFull" -ForegroundColor Green

# Export to CER (public cert for testers)
Export-Certificate -Cert $cert -FilePath $cerFull | Out-Null
Write-Host "Exported public cert: $cerFull" -ForegroundColor Green

if ($InstallLocal) {
  Write-Host "Installing to local machine Trusted Root & Trusted Publisher (requires admin)..." -ForegroundColor Cyan
  foreach ($storeName in 'Root', 'TrustedPublisher') {
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($storeName, 'LocalMachine')
    $store.Open('ReadWrite')
    $store.Add($cert)
    $store.Close()
  }
  Write-Host "Installed into LocalMachine\Root and LocalMachine\TrustedPublisher." -ForegroundColor Green
}

Write-Host "`nDone. To sign binaries, run:" -ForegroundColor Cyan
Write-Host "  .\scripts\sign-binaries.ps1 -PfxPath '$PfxPath' -Password '$Password'"
