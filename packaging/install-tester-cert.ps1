#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Installs or uninstalls the GamersEqualizer development certificate for testers.

.DESCRIPTION
  Imports GamersEqualizer-dev.cer into:
  - "Trusted Root Certification Authorities" (LocalMachine\Root)
  - "Trusted Publishers" (LocalMachine\TrustedPublisher)

  Once installed, Windows SmartScreen and Smart App Control recognize binaries signed with
  this certificate as trusted without warnings or blocks.

  Undo: .\install-tester-cert.ps1 -Uninstall

.PARAMETER CerPath
  Path to the .cer certificate file. Defaults to GamersEqualizer-dev.cer in the script folder.

.PARAMETER Uninstall
  Removes the GamersEqualizer development certificate from the certificate stores.

.EXAMPLE
  .\install-tester-cert.ps1
  .\install-tester-cert.ps1 -Uninstall
#>
param(
  [string]$CerPath,
  [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

if (-not $CerPath) {
  $candidate = Join-Path $PSScriptRoot 'GamersEqualizer-dev.cer'
  if (Test-Path $candidate) {
    $CerPath = $candidate
  } else {
    $CerPath = Join-Path $PSScriptRoot '..\packaging\GamersEqualizer-dev.cer'
  }
}

$stores = @('Root', 'TrustedPublisher')

if ($Uninstall) {
  Write-Host "Removing GamersEqualizer development certificate from certificate stores..." -ForegroundColor Cyan
  $removedCount = 0
  foreach ($storeName in $stores) {
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($storeName, 'LocalMachine')
    $store.Open('ReadWrite')
    $matches = @($store.Certificates | Where-Object { $_.Subject -like '*GamersEqualizer*' })
    foreach ($c in $matches) {
      $store.Remove($c)
      Write-Host "Removed $($c.Subject) ($($c.Thumbprint)) from LocalMachine\$storeName" -ForegroundColor Yellow
      $removedCount++
    }
    $store.Close()
  }
  if ($removedCount -eq 0) {
    Write-Host "No matching GamersEqualizer certificate found in stores." -ForegroundColor Gray
  } else {
    Write-Host "Certificate uninstalled." -ForegroundColor Green
  }
  return
}

# Install mode
if (-not (Test-Path $CerPath)) {
  throw "Certificate file not found: $CerPath"
}

$cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($CerPath)
Write-Host "Installing certificate:" -ForegroundColor Cyan
Write-Host "  Subject    : $($cert.Subject)"
Write-Host "  Thumbprint : $($cert.Thumbprint)"
Write-Host "  Expires    : $($cert.NotAfter)"

foreach ($storeName in $stores) {
  $store = New-Object System.Security.Cryptography.X509Certificates.X509Store($storeName, 'LocalMachine')
  $store.Open('ReadWrite')
  # Avoid adding duplicates
  $existing = @($store.Certificates | Where-Object { $_.Thumbprint -eq $cert.Thumbprint })
  if ($existing.Count -eq 0) {
    $store.Add($cert)
    Write-Host "Added to LocalMachine\$storeName" -ForegroundColor Green
  } else {
    Write-Host "Already present in LocalMachine\$storeName" -ForegroundColor Gray
  }
  $store.Close()
}

Write-Host "`nCertificate installed successfully." -ForegroundColor Green
Write-Host "GamersEqualizer builds signed with this certificate will run without SmartScreen warnings."
