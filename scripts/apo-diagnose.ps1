<#
.SYNOPSIS
  Read-only check: is the GamersEqualizer APO really loaded by Windows audio, and if not, why?
  Changes nothing.

.DESCRIPTION
  The app's own check only reads the registry (DLL present, our CLSID in the device's effect
  list). This also looks at what decides whether audiodg.exe really runs it: audio enhancements,
  processing modes of the effect slot, whether audiodg has the DLL loaded, whether the APO and
  the app share the same memory, antivirus detections, code integrity blocks, the app's log
  lines about the APO, and audiodg crashes.

  Before running: open an app that records (Sound Recorder) and play some sound, so audiodg runs
  the microphone and the output effects. Run as administrator (lists audiodg's modules).
    powershell -ExecutionPolicy Bypass -File apo-diagnose.ps1
#>

$ErrorActionPreference = 'Continue'

# Must match the CLSIDs in scripts/apo-install.ps1 (mic chain, output EQ, stream EQ).
$Ours = '17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea|6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10|3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47'
$Clsids = @('{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}', '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}', '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}')
$AudioRoot = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$Fx = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
# Processing modes each slot runs in: ,5 SFX ,6 MFX ,7 EFX.
$Modes = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$SysFxOff = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'
$NameProp = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
$DescProp = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$DllName = 'gamersequalizer_apo.dll'

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
  [Security.Principal.WindowsBuiltInRole]::Administrator)
$nt = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
"Windows $($nt.DisplayVersion) build $($nt.CurrentBuild).$($nt.UBR), administrator: $admin"
$audio = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio' -ErrorAction SilentlyContinue
"DisableProtectedAudioDG = $($audio.DisableProtectedAudioDG)"

"`n== DLL and COM registration"
$dll = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) "GamersEqualizer\apo\$DllName"
if (Test-Path $dll) {
  $file = Get-Item $dll
  "DLL: $dll, $($file.LastWriteTime), version '$($file.VersionInfo.ProductVersion)'"
  # A DLL from a downloaded ZIP keeps the internet zone mark unless the installer removed it.
  $zone = Get-Item $dll -Stream Zone.Identifier -ErrorAction SilentlyContinue
  "  internet zone mark: $([bool]$zone)"
}
else { "DLL: MISSING ($dll)" }
foreach ($clsid in $Clsids) {
  $server = (Get-ItemProperty "HKLM:\SOFTWARE\Classes\CLSID\$clsid\InprocServer32" -ErrorAction SilentlyContinue).'(default)'
  $apo = Test-Path "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$clsid"
  "$clsid -> '$server', APO registered: $apo"
}

"`n== Devices with our effect (state 1 = connected)"
$found = $false
foreach ($flow in 'Capture', 'Render') {
  foreach ($key in Get-ChildItem (Join-Path $AudioRoot $flow)) {
    $fxProps = Get-ItemProperty "$($key.PSPath)\FxProperties" -ErrorAction SilentlyContinue
    if (-not $fxProps) { continue }
    $lists = (13, 14, 15 | ForEach-Object { $fxProps."$Fx,$_" }) -join ' '
    if ($lists -notmatch $Ours) { continue }
    $found = $true
    $props = Get-ItemProperty "$($key.PSPath)\Properties" -ErrorAction SilentlyContinue
    $state = (Get-ItemProperty $key.PSPath).DeviceState
    "`n$flow $($key.PSChildName) state=$state : $($props.$NameProp) ($($props.$DescProp))"
    "  enhancements off flag: FxProperties=$($fxProps.$SysFxOff) Properties=$($props.$SysFxOff)"
    foreach ($n in 1, 2, 5, 6, 7, 13, 14, 15) {
      $value = $fxProps."$Fx,$n"
      if ($null -ne $value) { "  fx,$n = $($value -join ' | ')" }
    }
    foreach ($n in 5, 6, 7) { "  modes,$n = $($fxProps."$Modes,$n" -join ' | ')" }
  }
}
if (-not $found) { 'No device lists our effect: the install did not reach any device.' }

"`n== Is the DLL loaded in audiodg?"
if ($admin) {
  # Per process: capture and render may run in different audiodg instances.
  $withDll = @(tasklist /m $DllName /fo csv /nh 2>$null | Select-String -Pattern '"audiodg' | ForEach-Object { ($_.Line -split '","')[1] })
  foreach ($p in @(Get-Process audiodg -ErrorAction SilentlyContinue)) {
    "audiodg pid $($p.Id) since $($p.StartTime): $DllName loaded: $($withDll -contains "$($p.Id)")"
  }
  if (-not (Get-Process audiodg -ErrorAction SilentlyContinue)) { 'audiodg is not running (no stream open)' }
}
else { 'unknown: run as administrator' }

"`n== Shared memory between the APO and the app"
# The APO creates Global\GamersEqualizer_Apo_vN; the app connects only to the same N (bumped
# whenever the layout changes), so a DLL and an app from different builds never meet.
$pattern = 'GamersEqualizer_Apo_v\d+'
function Get-ShmName([string]$path) {
  [regex]::Match([Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($path)), $pattern).Value
}
$apoShm = if (Test-Path $dll) { Get-ShmName $dll } else { '' }
"installed DLL uses: $apoShm"
$apps = @(Get-Process gamersequalizer -ErrorAction SilentlyContinue)
if (-not $apps) { 'GamersEqualizer is not running' }
foreach ($app in $apps) {
  $version = (Get-Item $app.Path -ErrorAction SilentlyContinue).VersionInfo.ProductVersion
  "running pid $($app.Id) since $($app.StartTime), version '$version': $($app.Path) uses: $(Get-ShmName $app.Path)"
}
if ($apoShm) {
  try {
    $map = [IO.MemoryMappedFiles.MemoryMappedFile]::OpenExisting("Global\$apoShm", [IO.MemoryMappedFiles.MemoryMappedFileRights]::Read)
    $map.Dispose()
    "Global\$apoShm exists: True (the APO created it)"
  }
  catch { "Global\$apoShm exists: False ($($_.Exception.InnerException.Message))" }
}

"`n== Antivirus (Microsoft Defender) detections of our files"
try {
  $detections = Get-MpThreatDetection -ErrorAction Stop | Where-Object { ($_.Resources -join ' ') -match 'gamersequalizer' }
  if ($detections) {
    foreach ($d in $detections) {
      $threat = (Get-MpThreat -ThreatID $d.ThreatID -ErrorAction SilentlyContinue).ThreatName
      "$($d.InitialDetectionTime) $threat : $($d.Resources -join ', ')"
    }
  }
  else { 'none' }
}
catch { "unknown: $($_.Exception.Message)" }

"`n== Code integrity blocks (last 500 events)"
$ci = Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 500 -ErrorAction SilentlyContinue |
  Where-Object { $_.Message -match 'gamersequalizer' }
if ($ci) { $ci | Select-Object -First 5 | ForEach-Object { "$($_.TimeCreated) id $($_.Id): $($_.Message)" } }
else { 'none' }

"`n== Notes from inside audiodg (Application log, source GamersEqualizer, newest 100)"
# Written by APO builds that have this check (event_log.rs); older builds write none.
$notes = Get-WinEvent -LogName Application -FilterXPath "*[System[Provider[@Name='GamersEqualizer']]]" -MaxEvents 100 -ErrorAction SilentlyContinue
if ($notes) {
  $notes | Sort-Object TimeCreated | ForEach-Object { "$($_.TimeCreated) $($_.LevelDisplayName): $($_.Properties[0].Value)" }
}
else { 'none (an APO build without notes, or no stream started since it was installed)' }

"`n== App log: APO link, microphone and setup lines (last 40)"
# Elevated as another administrator account, APPDATA is that account's: then this finds nothing.
$appLog = Join-Path $env:APPDATA 'GamersEqualizer\logs\gamersequalizer.log'
if (Test-Path $appLog) {
  Select-String -Path $appLog -Pattern 'ApoLink|ApoCheck|ApoNote|ConsoleOwner|\[Mic\]|Denoise|Health|Spawning|\[Setup\]|Initializing' |
    Select-Object -Last 40 | ForEach-Object { $_.Line }
}
else { "not found: $appLog" }

"`n== audiodg crashes (last 7 days)"
$crashes = Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'Application Error'; StartTime = (Get-Date).AddDays(-7) } -ErrorAction SilentlyContinue |
  Where-Object { $_.Message -match 'audiodg' }
if ($crashes) { $crashes | Select-Object -First 5 | ForEach-Object { "$($_.TimeCreated): $(($_.Message -split "`n" | Select-Object -First 3) -join ' ')" } }
else { 'none' }
