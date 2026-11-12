<#
.SYNOPSIS
  Generates a self-signed code signing certificate for our own builds.

.DESCRIPTION
  Creates a self-signed Authenticode certificate and exports it with its private key to a .pfx
  (secrets\ by default, git-ignored) for scripts\sign-binaries.ps1 and the CI secret.

  Not for testers: SmartScreen and Smart App Control do not accept a self-signed root, so
  installing it on their PCs gains nothing. Testers use "More info -> Run anyway".

  The certificate is limited: end entity (BasicConstraints CA=false, so it cannot issue
  certificates), code signing EKU only, valid for 1 year. After the export its private key is
  removed from the certificate store; the password-protected .pfx is the only copy.

.PARAMETER Subject
  Certificate subject name. Default: "CN=GamersEqualizer Dev"

.PARAMETER PfxPath
  Path for the private .pfx file. Default: "secrets\GamersEqualizer-dev.pfx"

.PARAMETER InstallLocal
  Also trusts the certificate on this PC (LocalMachine Root + TrustedPublisher; needs admin),
  so signatures show as valid here.

.EXAMPLE
  .\scripts\create-dev-cert.ps1
  .\scripts\create-dev-cert.ps1 -InstallLocal
#>
param(
  [string]$Subject = 'CN=GamersEqualizer Dev',
  [string]$PfxPath = 'secrets\GamersEqualizer-dev.pfx',
  [switch]$InstallLocal
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $root

$pfxFull = if ([IO.Path]::IsPathRooted($PfxPath)) { $PfxPath } else { [IO.Path]::GetFullPath((Join-Path $root $PfxPath)) }
if (Test-Path $pfxFull) { throw "$pfxFull already exists; not overwriting it" }

# Asked, never defaulted or printed: a known password makes a copied .pfx worthless as a secret.
$password = Read-Host -AsSecureString 'Password for the .pfx'
$again = Read-Host -AsSecureString 'Same password again'
$plain = [Net.NetworkCredential]::new('', $password).Password
if (-not $plain) { throw 'An empty password is not allowed.' }
if ($plain -ne [Net.NetworkCredential]::new('', $again).Password) { throw 'The passwords differ.' }
$plain = $null

$pfxDir = [IO.Path]::GetDirectoryName($pfxFull)
if (-not (Test-Path $pfxDir)) { New-Item -ItemType Directory -Force $pfxDir | Out-Null }

Write-Host "Creating self-signed code signing certificate: $Subject..." -ForegroundColor Cyan
$cert = New-SelfSignedCertificate `
  -Type CodeSigningCert `
  -Subject $Subject `
  -KeyUsage DigitalSignature `
  -TextExtension @('2.5.29.19={critical}{text}ca=0', '2.5.29.37={text}1.3.6.1.5.5.7.3.3') `
  -FriendlyName 'GamersEqualizer Code Signing' `
  -NotAfter (Get-Date).AddYears(1) `
  -CertStoreLocation 'Cert:\CurrentUser\My'
if (-not $cert) { throw 'Failed to create self-signed certificate.' }

try {
  Export-PfxCertificate -Cert $cert -FilePath $pfxFull -Password $password | Out-Null
  Write-Host "Exported private key: $pfxFull" -ForegroundColor Green
  if ($InstallLocal) {
    $public = [Security.Cryptography.X509Certificates.X509Certificate2]::new($cert.RawData)
    foreach ($storeName in 'Root', 'TrustedPublisher') {
      $store = [Security.Cryptography.X509Certificates.X509Store]::new($storeName, 'LocalMachine')
      $store.Open('ReadWrite')
      try { $store.Add($public) } finally { $store.Close() }
    }
    Write-Host 'Trusted on this PC (LocalMachine\Root and TrustedPublisher).' -ForegroundColor Green
  }
} finally {
  # The .pfx is the only copy of the key; no exportable key stays behind in the store.
  Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($cert.Thumbprint)" -DeleteKey
}

Write-Host "  Thumbprint : $($cert.Thumbprint)"
Write-Host "  Expires    : $($cert.NotAfter)"
Write-Host "`nTo sign (asks for the password): .\scripts\sign-binaries.ps1 -PfxPath '$PfxPath'" -ForegroundColor Cyan
