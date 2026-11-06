<#
.SYNOPSIS
  Dev loop: stop the running app, build, check, optionally start it again.

.DESCRIPTION
  1. Stops every gamersequalizer.exe (the GUI and the background daemon) so the exe isn't locked.
  2. cargo build --release -p gamersequalizer (the app).
  3. -Apo: cargo build --profile apo -p gamersequalizer-apo (the microphone APO DLL, unwinding profile).
  4. -Test: cargo test --workspace, plus clippy on the workspace (warnings are listed, not fatal).
  5. Reports whether the installed APO (needs admin to install) is older than target\apo.
  6. -Start: starts target\release\gamersequalizer.exe.

  Exit code 0 = all requested steps passed. The last lines are a one-line-per-step summary.

.EXAMPLE
  .\scripts\dev-rebuild.ps1 -Test -Start
  .\scripts\dev-rebuild.ps1 -Apo -Test
#>
param(
  [switch]$Apo,
  [switch]$Test,
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

if (-not $failed) { Step 'build app (release)' { cargo build --release -p gamersequalizer } }
if ($Apo -and -not $failed) { Step 'build APO (apo profile)' { cargo build --profile apo -p gamersequalizer-apo } }

if ($Test -and -not $failed) {
  Step 'cargo test --workspace' { cargo test --workspace }
  Step 'clippy --workspace' {
    # Windows PowerShell turns redirected native stderr into error records; with 'Stop' the
    # first clippy warning would abort the step.
    $ErrorActionPreference = 'Continue'
    $out = cargo clippy --workspace --all-targets 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { Write-Host $out; throw 'clippy failed' }
    $warnings = ([regex]::Matches($out, '(?m)^warning: (?!`).*$')).Count
    Write-Host "clippy warnings: $warnings"
  }
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
