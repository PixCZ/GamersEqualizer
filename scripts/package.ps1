<#
.SYNOPSIS
  Builds a portable ZIP for testers:
  target\package\GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>[-dirty][-fast].zip

.DESCRIPTION
  Final build (default) — every step is checked, nothing is taken on trust:
  1. Clears environment overrides that would silently change the build (RUSTFLAGS,
     CARGO_PROFILE_*, CARGO_INCREMENTAL, CARGO_TARGET_DIR, ...) for this process.
  2. cargo clean --profile dist / apo-dist: everything is compiled from scratch, no stale objects.
  3. cargo build --locked -v --profile dist     -p gamersequalizer      (exe, panic = abort)
     cargo build --locked -v --profile apo-dist -p gamersequalizer-apo  (DLL, panic = unwind)
     Both: opt-level 3, fat LTO, codegen-units 1, no incremental, stripped, static CRT.
  4. Reads the real rustc command lines from the verbose log and fails unless the exe and the
     DLL were linked with exactly those flags (and without the debug-capture feature).
  5. Checks both binaries were written by this build and import no VCRUNTIME140.dll.
  6. Stages gamersequalizer.exe + gamersequalizer_apo.dll + LICENSE.txt and zips them under a new,
     time-stamped name. Next to the ZIP: <name>.build.log (full cargo output) and
     <name>.build.txt (toolchain, commit, flags, SHA256 of each file).
     Older ZIPs are never deleted or overwritten; only target\package\staging is cleared.

  -Fast packages the dev builds instead (release + apo profiles, no LTO, seconds, no checks
  beyond the VCRUNTIME one). Never for testers' final builds.

  Portable ZIP, not a setup.exe: the app already installs/uninstalls its system parts itself.
  -Sign (or auto-detected secrets\GamersEqualizer-dev.pfx / $env:GAMERSEQUALIZER_PFX_PATH) signs
  the staged binaries with Authenticode SHA256 (scripts\sign-binaries.ps1, asks for the .pfx
  password); -Sign fails the build when signing or timestamping fails. Without a .pfx and
  without -Sign the ZIP is unsigned. No certificate goes into the ZIP: testers do not install one.

  Known limit: generic x86-64 code (no target-cpu=native / AVX2), on purpose — it must run on
  any tester PC. Upgrade only with a CPU check at startup.

.EXAMPLE
  .\scripts\package.ps1          # final build for testers (from scratch, fat LTO, ~8 min)
  .\scripts\package.ps1 -Fast    # from the dev release build (also: dev-rebuild.ps1 -Package)
  .\scripts\package.ps1 -Sign    # sign binaries using cert in secrets\ or Cert:\CurrentUser\My
#>
param(
  [switch]$Fast,
  [switch]$Sign,
  [switch]$NoSign,
  [string]$PfxPath,
  [string]$PackageName
)

$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
Set-Location $root

$appProfile = if ($Fast) { 'release' } else { 'dist' }
$apoProfile = if ($Fast) { 'apo' } else { 'apo-dist' }

# Flags the final rustc call of each binary must carry (verbose cargo output, see step 4).
$commonFlags = @('-C opt-level=3', '-C lto=fat', '-C codegen-units=1', '-C strip=symbols',
  '-C target-feature=+crt-static', '-C control-flow-guard=checks')

function Invoke-Cargo([string[]]$cargoArgs) {
  & cargo @cargoArgs
  if ($LASTEXITCODE -ne 0) { throw "cargo $($cargoArgs -join ' ') failed (exit $LASTEXITCODE)" }
}

# Full verbose output goes to the log; the console skips the long rustc command lines.
function Invoke-CargoLogged([string[]]$cargoArgs, [string]$log) {
  Add-Content -LiteralPath $log -Value "`n> cargo $($cargoArgs -join ' ')"
  $ErrorActionPreference = 'Continue' # cargo writes progress to stderr; that is not an error
  # No colours: CI sets CARGO_TERM_COLOR=always (dtolnay/rust-toolchain), and the ANSI codes
  # would hide the "Running `rustc ...`" lines from the filter below and from Assert-RustcFlags.
  & cargo @cargoArgs --color never 2>&1 | ForEach-Object {
    $line = "$_"
    Add-Content -LiteralPath $log -Value $line
    if ($line -notmatch '^\s+Running `') { Write-Host $line }
  }
  if ($LASTEXITCODE -ne 0) { throw "cargo $($cargoArgs -join ' ') failed (exit $LASTEXITCODE), log: $log" }
}

# The one rustc call that produced $crate as $crateType, checked flag by flag.
function Assert-RustcFlags([string]$log, [string]$crate, [string]$crateType, [string[]]$required, [string[]]$forbidden) {
  $calls = @(Get-Content -LiteralPath $log | Where-Object {
      $_ -match '^\s+Running `' -and $_ -match "--crate-name $crate\s" -and $_ -match "--crate-type $crateType\s"
    })
  if ($calls.Count -ne 1) { throw "expected 1 rustc call for $crate ($crateType), found $($calls.Count); log: $log" }
  $call = $calls[0]
  foreach ($flag in $required) {
    if ($call -notmatch "(^|\s)$([regex]::Escape($flag))(\s|``|$)") { throw "$crate was built without '$flag'; log: $log" }
  }
  foreach ($flag in $forbidden) {
    if ($call -match [regex]::Escape($flag)) { throw "$crate was built with '$flag'; log: $log" }
  }
  Write-Host "OK  $crate ($crateType): $($required -join ', ')"
}

# Same rule as the binaries (crates\build-support\version_info.rs): the tag's version in a release
# build, otherwise "dev" (the commit follows in the name).
$version = if ($env:GAMERSEQUALIZER_RELEASE_VERSION) { $env:GAMERSEQUALIZER_RELEASE_VERSION.Trim().TrimStart('v') } else { 'dev' }
$commit = (git rev-parse --short HEAD).Trim()
# Untracked files do not count, as for the binaries' version.
$dirty = [bool](git status --porcelain --untracked-files=no)
if ($dirty) { $commit += '-dirty' }
if ($PackageName) {
  $name = $PackageName
} else {
  $name = "GamersEqualizer-$version-$(Get-Date -Format 'yyyyMMdd-HHmmss')-$commit"
  if ($Fast) { $name += '-fast' }
}

$out = Join-Path $root 'target\package'
$zip = Join-Path $out "$name.zip"
if (Test-Path $zip) { throw "$zip already exists; not overwriting it" }
New-Item -ItemType Directory -Force $out | Out-Null
$log = Join-Path $out "$name.build.log"
$buildStart = Get-Date

if ($Fast) {
  Invoke-Cargo @('build', '--profile', $appProfile, '-p', 'gamersequalizer')
  Invoke-Cargo @('build', '--profile', $apoProfile, '-p', 'gamersequalizer-apo')
} else {
  # 1. Overrides that beat Cargo.toml / .cargo\config.toml. RUSTFLAGS alone would drop +crt-static.
  $overrides = @(Get-ChildItem Env: | Where-Object {
      ($_.Name -in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'CARGO_INCREMENTAL',
      'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_TARGET', 'RUSTC', 'RUSTC_WRAPPER',
      'RUSTC_WORKSPACE_WRAPPER', 'CARGO_BUILD_RUSTC_WRAPPER', 'RUSTUP_TOOLCHAIN')) -or
      $_.Name -like 'CARGO_PROFILE_*' -or $_.Name -like 'CARGO_TARGET_*_RUSTFLAGS'
    })
  foreach ($var in $overrides) {
    Write-Warning "ignoring $($var.Name)=$($var.Value) for this build"
    Remove-Item "Env:$($var.Name)"
  }
  if ($dirty) { Write-Warning 'uncommitted changes: the ZIP is marked -dirty and does not match the commit' }

  $toolchain = (& rustc -Vv) -join "`n"
  Set-Content -LiteralPath $log -Value "$name`n$toolchain"

  # 2. From scratch: only these two profile folders, never the dev build or older ZIPs.
  Invoke-Cargo @('clean', '--profile', $appProfile)
  Invoke-Cargo @('clean', '--profile', $apoProfile)

  # 3. --locked: exactly the versions in Cargo.lock. No --features: the APO without debug-capture.
  Invoke-CargoLogged @('build', '--locked', '-v', '--profile', $appProfile, '-p', 'gamersequalizer') $log
  Invoke-CargoLogged @('build', '--locked', '-v', '--profile', $apoProfile, '-p', 'gamersequalizer-apo') $log

  # 4. What rustc was really told to do.
  $forbidden = @('-C incremental', 'debug-assertions=on', 'overflow-checks=on', 'feature="debug-capture"',
    'feature=\"debug-capture\"')
  Assert-RustcFlags $log 'gamersequalizer' 'bin' ($commonFlags + '-C panic=abort') $forbidden
  Assert-RustcFlags $log 'gamersequalizer_apo' 'cdylib' $commonFlags ($forbidden + '-C panic=abort')
}

$exe = "target\$appProfile\gamersequalizer.exe"
$dll = "target\$apoProfile\gamersequalizer_apo.dll"

# 5. Built just now, not left over from an earlier run (the final build only; -Fast reuses).
if (-not $Fast) {
  foreach ($bin in $exe, $dll) {
    if ((Get-Item $bin).LastWriteTime -lt $buildStart) { throw "$bin is older than this build" }
  }
}

# Scratch folder only (never the ZIPs next to it).
$staging = Join-Path $out 'staging'
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
$stage = Join-Path $staging $name
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item $exe $stage
Copy-Item $dll $stage
# GPL: the licence text travels with the binaries.
Copy-Item 'LICENSE' (Join-Path $stage 'LICENSE.txt')

# Sign the staged binaries on -Sign, or without asking when a .pfx is around and its password is
# in GAMERSEQUALIZER_PFX_PASSWORD (a password prompt would hang dev-rebuild.ps1 -Package).
if ($Sign -and $NoSign) { throw '-Sign and -NoSign contradict each other' }
$pfxAround = $PfxPath -or $env:GAMERSEQUALIZER_PFX_PATH -or (Test-Path (Join-Path $root 'secrets\GamersEqualizer-dev.pfx'))
if ($pfxAround -and -not $Sign -and -not $NoSign -and -not $env:GAMERSEQUALIZER_PFX_PASSWORD) {
  Write-Host "Not signing: a .pfx is there but GAMERSEQUALIZER_PFX_PASSWORD is not set (or pass -Sign)." -ForegroundColor Yellow
  $pfxAround = $false
}
if (($Sign -or $pfxAround) -and -not $NoSign) {
  Write-Host "`nSigning staged binaries..." -ForegroundColor Cyan
  $signParams = @{ Files = @((Join-Path $stage 'gamersequalizer.exe'), (Join-Path $stage 'gamersequalizer_apo.dll')) }
  if ($PfxPath) { $signParams.PfxPath = $PfxPath }
  if ($Sign) { $signParams.RequireTimestamp = $true }
  # Throws on any failure (missing script, wrong password, no certificate, no timestamp).
  & (Join-Path $PSScriptRoot 'sign-binaries.ps1') @signParams
}

# Fails loudly if static CRT linking (.cargo\config.toml) ever stops applying.
foreach ($bin in 'gamersequalizer.exe', 'gamersequalizer_apo.dll') {
  $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $stage $bin)))
  if ($text -match '(?i)vcruntime140\.dll') { throw "$bin still needs VCRUNTIME140.dll (Visual C++ runtime)" }
  # The linker sets IMAGE_DLLCHARACTERISTICS_GUARD_CF only when it really built the CFG table.
  $bytes = [IO.File]::ReadAllBytes((Join-Path $stage $bin))
  $dllCharacteristics = [BitConverter]::ToUInt16($bytes, [BitConverter]::ToInt32($bytes, 0x3C) + 24 + 70)
  if (-not ($dllCharacteristics -band 0x4000)) { throw "$bin was linked without Control Flow Guard" }
  # The mic chain's old VST2 entry point must not come back as an export of the APO DLL.
  if ($text -match 'VSTPluginMain') { throw "$bin exports VSTPluginMain" }
}

Compress-Archive -Path $stage -DestinationPath $zip
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash

if (-not $Fast) {
  $files = Get-ChildItem $stage | ForEach-Object { "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash, $_.Name }
  @(
    $name
    "commit   $(git rev-parse HEAD)$(if ($dirty) { ' (uncommitted changes)' })"
    "exe      profile ${appProfile}: $(($commonFlags + '-C panic=abort') -join ' ')"
    "apo dll  profile ${apoProfile}: $($commonFlags -join ' ') (panic=unwind)"
    ''
    $toolchain
    ''
    $files
    "$hash  $name.zip"
  ) | Set-Content -LiteralPath (Join-Path $out "$name.build.txt")
}

Write-Host "`n$zip"
Write-Host ("{0:N1} MB, SHA256 {1}" -f ((Get-Item $zip).Length / 1MB), $hash)
if (-not $Fast) { Write-Host "build log and info: $log, $(Join-Path $out "$name.build.txt")" }
