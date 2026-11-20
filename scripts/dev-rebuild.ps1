<#
.SYNOPSIS
  Dev loop: stop the running app, build, check, optionally start it again.

.DESCRIPTION
  1. Stops every gamersequalizer.exe (the GUI and the background daemon) so the exe isn't locked.
  2. cargo build --release -p gamersequalizer (the app).
  3. -Apo: cargo build --profile apo -p gamersequalizer-apo (the microphone APO DLL, unwinding profile).
     -DebugCapture (with -Apo): builds it with the `debug-capture` feature, so the mic chain records
     every stage on request (cargo run --release -p gamersequalizer-apo --example debug_capture).
     -CpuStats: builds the app (and with -Apo the APO) with the `cpu-stats` feature: once a minute the
     debug log ("Save debug to disk" on the Log page) gets `[Cpu]` lines with the CPU time of every
     thread of the service and of the window, and of our effects inside audiodg.
  4. -Test: the quality gate, scripts\check.ps1 (format, clippy at zero warnings, all tests).
  5. -Package: tester ZIP from this release build (scripts\package.ps1 -Fast, a few seconds).
     The final build for testers is scripts\package.ps1 without -Fast (fat LTO, minutes).
  6. Reports whether the installed APO (needs admin to install) is older than target\apo.
  7. -Start: starts target\release\gamersequalizer.exe.

  Exit code 0 = all requested steps passed. The last lines are a one-line-per-step summary.

.EXAMPLE
  .\scripts\dev-rebuild.ps1 -Test -Start
  .\scripts\dev-rebuild.ps1 -Apo -Test
  .\scripts\dev-rebuild.ps1 -Apo -DebugCapture
  .\scripts\dev-rebuild.ps1 -Apo -CpuStats -Start
  .\scripts\dev-rebuild.ps1 -Apo -Test -Package
#>
param(
  [switch]$Apo,
  [switch]$DebugCapture,
  [switch]$CpuStats,
  [switch]$Test,
  [switch]$Package,
  [switch]$Start
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $root
$summary = [System.Collections.Generic.List[string]]::new()
$failed = $false

function Step([string]$name, [scriptblock]$body) {
  Write-Host "`n=== $name ===" -ForegroundColor Cyan
  $t = [Diagnostics.Stopwatch]::StartNew()
  try {
    & $body
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
    $summary.Add("OK    $name ($([int]$t.Elapsed.TotalSeconds) s)")
  } catch {
    $script:failed = $true
    $summary.Add("FAIL  $name : $_")
  }
}

Step 'stop app' {
  $procs = @(Get-Process gamersequalizer -ErrorAction SilentlyContinue)
  foreach ($p in $procs) { Stop-Process -Id $p.Id -Force }
  foreach ($p in $procs) { $p.WaitForExit(30000) | Out-Null }
  # A service still exiting holds its mutex: a UI started now would not start a new service.
  $left = @(Get-Process gamersequalizer -ErrorAction SilentlyContinue)
  if ($left.Count) { throw "$($left.Count) process(es) still running after 30 s: $($left.Id -join ', ')" }
  Write-Host "stopped $($procs.Count) process(es)"
  $global:LASTEXITCODE = 0
}

# Diagnostic features: never packaged (below).
$appFeatures = @()
$apoFeatures = @()
if ($CpuStats) { $appFeatures += 'cpu-stats'; $apoFeatures += 'cpu-stats' }
if ($DebugCapture) { $apoFeatures += 'debug-capture' }
$appFeatureArgs = if ($appFeatures) { @('--features', ($appFeatures -join ',')) } else { @() }
$apoFeatureArgs = if ($apoFeatures) { @('--features', ($apoFeatures -join ',')) } else { @() }
$appBuild = "build app (release) $appFeatures".TrimEnd()
$apoBuild = "build APO (apo profile) $apoFeatures".TrimEnd()

if (-not $failed) {
  Step $appBuild { cargo build --release -p gamersequalizer @appFeatureArgs }
}
if ($DebugCapture -and -not $Apo) { $summary.Add('INFO  -DebugCapture only applies with -Apo') }
if ($CpuStats -and -not $Apo) { $summary.Add('INFO  -CpuStats without -Apo: the effects in audiodg report only from an APO built with it') }
if ($Apo -and -not $failed) {
  Step $apoBuild { cargo build --profile apo -p gamersequalizer-apo @apoFeatureArgs }
}

if ($Test -and -not $failed) {
  Step 'check (scripts\check.ps1)' { & (Join-Path $PSScriptRoot 'check.ps1') }
}

if ($Package -and ($DebugCapture -or $CpuStats)) {
  # package.ps1 would rebuild without the diagnostic features and replace this build.
  $summary.Add('INFO  -Package skipped: never package a -DebugCapture or -CpuStats build')
} elseif ($Package -and -not $failed) {
  Step 'package (tester ZIP, release build)' { & (Join-Path $PSScriptRoot 'package.ps1') -Fast }
}

$installed = Join-Path $env:ProgramFiles 'GamersEqualizer\apo\gamersequalizer_apo.dll'
$built = Join-Path $root 'target\apo\gamersequalizer_apo.dll'
if (-not (Test-Path $installed)) {
  $summary.Add('INFO  APO not installed (use the app banner or scripts\apo-install.ps1 as admin)')
} elseif ((Test-Path $built) -and (Get-Item $built).LastWriteTime -gt (Get-Item $installed).LastWriteTime) {
  $summary.Add('INFO  installed APO is older than target\apo: click Update in the app (admin)')
} else {
  $summary.Add('INFO  installed APO is current')
}

if ($Start -and -not $failed) {
  Step 'start app' { Start-Process (Join-Path $root 'target\release\gamersequalizer.exe'); $global:LASTEXITCODE = 0 }
}

Write-Host "`n=== SUMMARY ===" -ForegroundColor Cyan
$summary | ForEach-Object { Write-Host $_ }
if ($failed) { exit 1 } else { exit 0 }
