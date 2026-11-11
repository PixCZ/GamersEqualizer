<#
.SYNOPSIS
  Signs GamersEqualizer binaries with a Code Signing certificate (Authenticode SHA256).

.DESCRIPTION
  Signs one or more files (.exe, .dll, .ps1) using PowerShell's native Set-AuthenticodeSignature
  (no Windows SDK / signtool.exe installation required).

  Certificate lookup priority:
  1. -PfxPath parameter (or $env:GAMERSEQUALIZER_PFX_PATH)
  2. secrets\GamersEqualizer-dev.pfx (if it exists)
  3. -Thumbprint parameter in Cert:\CurrentUser\My
  4. Auto-discovery in Cert:\CurrentUser\My matching "GamersEqualizer*" with private key

.PARAMETER Files
  Files to sign. Defaults to target\release\gamersequalizer.exe and target\apo\gamersequalizer_apo.dll
  (or target\dist if release is absent).

.PARAMETER PfxPath
  Path to the .pfx file containing the private key.

.PARAMETER Password
  Password for the .pfx file (or uses $env:GAMERSEQUALIZER_PFX_PASSWORD, or default 'GamersEqualizer').

.PARAMETER Thumbprint
  Thumbprint of a certificate in Cert:\CurrentUser\My (alternative to .pfx).

.PARAMETER TimestampServer
  RFC 3161 / Authenticode timestamp server URL. Default: http://timestamp.digicert.com

.PARAMETER NoTimestamp
  Skip timestamping (for offline builds).

.PARAMETER CerOutPath
  Optional path to export the public .cer certificate matching the signing certificate.

.EXAMPLE
  .\scripts\sign-binaries.ps1
  .\scripts\sign-binaries.ps1 -Files target\dist\gamersequalizer.exe, target\apo-dist\gamersequalizer_apo.dll
  .\scripts\sign-binaries.ps1 -PfxPath 'D:\Certs\my-cert.pfx' -Password 'secret'
#>
param(
  [string[]]$Files,
  [string]$PfxPath,
  [string]$Password,
  [string]$Thumbprint,
  [string]$TimestampServer = 'http://timestamp.digicert.com',
  [switch]$NoTimestamp,
  [string]$CerOutPath
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')

# 1. Resolve target files
if (-not $Files -or $Files.Count -eq 0) {
  $candidates = @(
    (Join-Path $root 'target\release\gamersequalizer.exe'),
    (Join-Path $root 'target\apo\gamersequalizer_apo.dll')
  )
  $Files = @($candidates | Where-Object { Test-Path $_ })
  if ($Files.Count -eq 0) {
    throw 'No target binaries found (pass -Files explicitly or build first).'
  }
}

# 2. Load certificate
$cert = $null

if (-not $PfxPath -and $env:GAMERSEQUALIZER_PFX_PATH) {
  $PfxPath = $env:GAMERSEQUALIZER_PFX_PATH
}
if (-not $PfxPath) {
  $defaultPfx = Join-Path $root 'secrets\GamersEqualizer-dev.pfx'
  if (Test-Path $defaultPfx) { $PfxPath = $defaultPfx }
}

if ($PfxPath) {
  $pfxFull = if ([IO.Path]::IsPathRooted($PfxPath)) { $PfxPath } else { Join-Path $root $PfxPath }
  if (-not (Test-Path $pfxFull)) { throw "PFX file not found: $pfxFull" }
  $flags = [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::Exportable
  $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2
  $candidates = if ($PSBoundParameters.ContainsKey('Password')) {
    @($Password)
  } elseif ($env:GAMERSEQUALIZER_PFX_PASSWORD) {
    @($env:GAMERSEQUALIZER_PFX_PASSWORD)
  } else {
    @('', 'GamersEqualizer')
  }
  $imported = $false
  $lastErr = $null
  foreach ($pwd in $candidates) {
    try {
      $cert.Import($pfxFull, $pwd, $flags)
      $imported = $true
      break
    } catch {
      $lastErr = $_
    }
  }
  if (-not $imported) { throw "Failed to import PFX ($pfxFull): $lastErr" }
  Write-Host "Using PFX certificate: $($cert.Subject) ($($cert.Thumbprint))" -ForegroundColor Cyan
}
elseif ($Thumbprint) {
  $cert = Get-Item "Cert:\CurrentUser\My\$Thumbprint" -ErrorAction SilentlyContinue
  if (-not $cert) { throw "Certificate with thumbprint $Thumbprint not found in Cert:\CurrentUser\My" }
  Write-Host "Using store certificate: $($cert.Subject) ($($cert.Thumbprint))" -ForegroundColor Cyan
}
else {
  $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert |
    Where-Object { $_.Subject -like '*GamersEqualizer*' -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1
  if (-not $cert) {
    throw "No signing certificate found. Run .\scripts\create-dev-cert.ps1 first or pass -PfxPath."
  }
  Write-Host "Using store certificate: $($cert.Subject) ($($cert.Thumbprint))" -ForegroundColor Cyan
}

if (-not $cert.HasPrivateKey) {
  throw "Certificate does not contain a private key: $($cert.Subject)"
}

if ($CerOutPath) {
  $cerFull = if ([IO.Path]::IsPathRooted($CerOutPath)) { $CerOutPath } else { Join-Path $root $CerOutPath }
  Export-Certificate -Cert $cert -FilePath $cerFull | Out-Null
  Write-Host "Exported public cert: $cerFull" -ForegroundColor Green
}

# 3. Sign each file
foreach ($file in $Files) {
  $fullPath = if ([IO.Path]::IsPathRooted($file)) { $file } else { Join-Path $root $file }
  if (-not (Test-Path $fullPath)) { throw "File not found: $fullPath" }

  $sig = $null
  if (-not $NoTimestamp -and $TimestampServer) {
    try {
      $sig = Set-AuthenticodeSignature -FilePath $fullPath -Certificate $cert -HashAlgorithm SHA256 -TimestampServer $TimestampServer -ErrorAction Stop
    }
    catch {
      Write-Warning "Timestamping failed ($TimestampServer): $_"
    }
  }
  if (-not $sig -or -not $sig.SignerCertificate) {
    $sig = Set-AuthenticodeSignature -FilePath $fullPath -Certificate $cert -HashAlgorithm SHA256
  }

  $verify = Get-AuthenticodeSignature -FilePath $fullPath
  if (-not $verify.SignerCertificate) {
    throw "Failed to sign $fullPath (status: $($verify.Status), message: $($verify.StatusMessage))"
  }

  $statusNote = if ($verify.Status -eq 'Valid') { 'Valid (Trusted)' } else { "$($verify.Status) (self-signed not in Trusted Root)" }
  Write-Host "Signed $([IO.Path]::GetFileName($fullPath)): $statusNote" -ForegroundColor Green
}
