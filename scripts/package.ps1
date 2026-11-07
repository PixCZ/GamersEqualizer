<#
.SYNOPSIS
  Builds a portable ZIP for testers:
  target\package\GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>[-dirty][-fast].zip

.DESCRIPTION
  1. Builds the app: --profile dist (whole-program optimized, minutes) or, with -Fast, the
     --release build the dev loop already made (seconds when it is up to date).
  2. cargo build --profile apo -p gamersequalizer-apo (the APO DLL, unwinding profile).
  3. Stages the layout the app looks for next to its exe (win_audio/apo_install.rs):
       gamersequalizer.exe      (apo-install.ps1 / apo-uninstall.ps1 are built into it)
       gamersequalizer_apo.dll  (loaded by audiodg from a file, so it stays separate)
       CTI-ME.txt               (tester instructions, packaging\CTI-ME.txt)
  4. Zips it under a new, time-stamped name. Older ZIPs are never deleted or overwritten;
     only the scratch folder target\package\staging is cleared.

  Portable ZIP, not a setup.exe: the app already installs/uninstalls its system parts itself.
  Upgrade to a real installer (Inno Setup / WiX) when it should land in Program Files with a
  Start menu entry. Binaries are unsigned: testers see a SmartScreen prompt.

.EXAMPLE
  .\scripts\package.ps1          # final build for testers (slow, fat LTO)
  .\scripts\package.ps1 -Fast    # from the dev release build (also: dev-rebuild.ps1 -Package)
#>
param(
  [switch]$Fast
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $root

function Invoke-Cargo([string[]]$cargoArgs) {
  & cargo @cargoArgs
  if ($LASTEXITCODE -ne 0) { throw "cargo $($cargoArgs -join ' ') failed (exit $LASTEXITCODE)" }
}

$appProfile = if ($Fast) { 'release' } else { 'dist' }
Invoke-Cargo @('build', '--profile', $appProfile, '-p', 'gamersequalizer')
Invoke-Cargo @('build', '--profile', 'apo', '-p', 'gamersequalizer-apo')

$version = (Select-String -Path 'crates\gamersequalizer-daemon\Cargo.toml' -Pattern '^version\s*=\s*"(.+)"' |
  Select-Object -First 1).Matches[0].Groups[1].Value
$commit = (git rev-parse --short HEAD).Trim()
if (git status --porcelain) { $commit += '-dirty' }
$name = "GamersEqualizer-$version-$(Get-Date -Format 'yyyyMMdd-HHmmss')-$commit"
if ($Fast) { $name += '-fast' }

$out = Join-Path $root 'target\package'
$zip = Join-Path $out "$name.zip"
if (Test-Path $zip) { throw "$zip already exists; not overwriting it" }

# Scratch folder only (never the ZIPs next to it).
$staging = Join-Path $out 'staging'
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
$stage = Join-Path $staging $name
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item "target\$appProfile\gamersequalizer.exe" $stage
Copy-Item 'target\apo\gamersequalizer_apo.dll' $stage
Copy-Item 'packaging\CTI-ME.txt' $stage

# Fails loudly if static CRT linking (.cargo\config.toml) ever stops applying.
foreach ($bin in 'gamersequalizer.exe', 'gamersequalizer_apo.dll') {
  $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $stage $bin)))
  if ($text -match '(?i)vcruntime140\.dll') { throw "$bin still needs VCRUNTIME140.dll (Visual C++ runtime)" }
}

Compress-Archive -Path $stage -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
Write-Host "`n$zip"
Write-Host ("{0:N1} MB, SHA256 {1}" -f ((Get-Item $zip).Length / 1MB), $hash)
