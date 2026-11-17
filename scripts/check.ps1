<#
.SYNOPSIS
  The quality gate: format, clippy, tests. Work is done when this passes.

.DESCRIPTION
  Run by scripts\dev-rebuild.ps1 -Test and by CI (.github/workflows/ci.yml) on every push.
  Stops at the first failing step with a non-zero exit code. Windows only (the workspace builds
  only there); off Windows run just the installer self-check: pwsh scripts/tests/apo-install.tests.ps1

.EXAMPLE
  .\scripts\check.ps1
#>
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..'))
$summary = [System.Collections.Generic.List[string]]::new()

function Gate([string]$name, [scriptblock]$body) {
  Write-Host "`n=== $name ===" -ForegroundColor Cyan
  $t = [Diagnostics.Stopwatch]::StartNew()
  # Windows PowerShell turns native stderr into error records; with 'Stop' the first line cargo
  # writes there would end the step.
  $ErrorActionPreference = 'Continue'
  $global:LASTEXITCODE = 0
  try {
    & $body
    if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
  } catch {
    $summary | ForEach-Object { Write-Host $_ }
    Write-Host "FAIL  $name : $_" -ForegroundColor Red
    exit 1
  }
  $summary.Add("OK    $name ($([int]$t.Elapsed.TotalSeconds) s)")
}

Gate 'fmt' { cargo fmt -- --check }
Gate 'clippy --workspace' { cargo clippy --workspace --all-targets -- -D warnings }
Gate 'clippy apo (debug-capture)' { cargo clippy -p gamersequalizer-apo --all-targets --features debug-capture -- -D warnings }
Gate 'test --workspace' { cargo test --workspace }
# Throws when a check fails.
Gate 'installer self-check' { & (Join-Path $PSScriptRoot 'tests\apo-install.tests.ps1') }
# The installer runs under Windows PowerShell 5.1 on users' PCs; PowerShell 7 (CI) accepts syntax
# it does not have. Under 5.1 already, the gate above was that run.
if ($PSVersionTable.PSEdition -eq 'Core') {
  Gate 'installer self-check (Windows PowerShell 5.1)' {
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'tests\apo-install.tests.ps1')
  }
}
# Vendor forks are outside the workspace. rnnoise2: its AVX2-vs-scalar kernel test and the upstream
# reference test need x86_64. silero_vad: the port against onnxruntime's output.
Gate 'test rnnoise2 (release)' { cargo test --release --manifest-path crates/vendor/rnnoise2/Cargo.toml }
Gate 'test silero_vad (release)' { cargo test --release --manifest-path crates/vendor/silero_vad/Cargo.toml }
# deep_filter: the f16 weight conversion (kernels picked, output close to f32). Standalone it builds
# against tract from crates.io, not the workspace's patched one; the conversion is the same.
Gate 'test deep_filter (release)' { cargo test --release --manifest-path crates/vendor/deep_filter/Cargo.toml --features tract,default-model }
# Fails only on an error: warnings in the forked code are not ours to fix.
Gate 'clippy rnnoise2' { cargo clippy --manifest-path crates/vendor/rnnoise2/Cargo.toml --all-targets }

Write-Host "`n=== CHECK SUMMARY ===" -ForegroundColor Cyan
$summary | ForEach-Object { Write-Host $_ }
exit 0
