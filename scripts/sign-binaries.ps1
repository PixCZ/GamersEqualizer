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
  Password for the .pfx file. Otherwise $env:GAMERSEQUALIZER_PFX_PASSWORD (CI), otherwise it is
  asked for. There is no default password.

.PARAMETER Thumbprint
  Thumbprint of a certificate in Cert:\CurrentUser\My (alternative to .pfx).

.PARAMETER TimestampServer
  RFC 3161 / Authenticode timestamp server URL. Default: http://timestamp.digicert.com

.PARAMETER NoTimestamp
  Skip timestamping (for offline builds). Such a signature turns invalid when the
  certificate expires.

.PARAMETER RequireTimestamp
  Fail instead of signing without a timestamp when the timestamp server fails. Always on in CI
  ($env:CI set); scripts\package.ps1 -Sign sets it.

.EXAMPLE
  .\scripts\sign-binaries.ps1
  .\scripts\sign-binaries.ps1 -Files target\dist\gamersequalizer.exe, target\apo-dist\gamersequalizer_apo.dll
  .\scripts\sign-binaries.ps1 -PfxPath 'D:\Certs\my-cert.pfx'   # asks for the password
#>
param(
  [string[]]$Files,
  [string]$PfxPath,
  [string]$Password,
  [string]$Thumbprint,
  [string]$TimestampServer = 'http://timestamp.digicert.com',
  [switch]$NoTimestamp,
  [switch]$RequireTimestamp
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
  # One password, never guessed: a parameter, the CI secret, or asked for.
  $secure = if ($PSBoundParameters.ContainsKey('Password')) {
    ConvertTo-SecureString -String $Password -AsPlainText -Force
  } elseif ($env:GAMERSEQUALIZER_PFX_PASSWORD) {
    ConvertTo-SecureString -String $env:GAMERSEQUALIZER_PFX_PASSWORD -AsPlainText -Force
  } else {
    Read-Host -AsSecureString "Password for $pfxFull"
  }
  $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2
  try {
    $cert.Import($pfxFull, $secure, [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::DefaultKeySet)
  } catch {
    throw "Failed to import PFX ($pfxFull), wrong password?: $_"
  }
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

$RequireTimestamp = $RequireTimestamp -or [bool]$env:CI
if ($NoTimestamp -and $RequireTimestamp) { throw '-NoTimestamp contradicts -RequireTimestamp (or CI).' }

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
      if ($RequireTimestamp) { throw "Timestamping failed ($TimestampServer): $_" }
      Write-Warning "Timestamping failed ($TimestampServer), signing without a timestamp: $_"
    }
  }
  if (-not $sig -or -not $sig.SignerCertificate) {
    $sig = Set-AuthenticodeSignature -FilePath $fullPath -Certificate $cert -HashAlgorithm SHA256
  }

  $verify = Get-AuthenticodeSignature -FilePath $fullPath
  if (-not $verify.SignerCertificate) {
    throw "Failed to sign $fullPath (status: $($verify.Status), message: $($verify.StatusMessage))"
  }
  if ($RequireTimestamp -and -not $verify.TimeStamperCertificate) { throw "$fullPath has no timestamp" }

  $statusNote = if ($verify.Status -eq 'Valid') { 'Valid (Trusted)' } else { "$($verify.Status) (self-signed not in Trusted Root)" }
  Write-Host "Signed $([IO.Path]::GetFileName($fullPath)): $statusNote" -ForegroundColor Green
}
