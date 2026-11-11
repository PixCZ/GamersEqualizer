#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Installs GamersEqualizer APOs on one or more physical endpoints in one go:
  -Flow Capture (default): the mic chain (AEC, noise suppression, EQ, boost) on a microphone.
  -Flow Render: the output EQ (headphone correction + game profile, EFX) plus the stream EQ
    (chat / media profiles per app stream, SFX) on headphones/speakers.
  -Flow Render -Slot SFX: the stream EQ only.  -NoStreamEq: the output EQ only.
  -Endpoint 'Capture:{guid}','Render:{guid}': several endpoints at once (default slots). The
    app uses this so the user approves one administrator prompt and audio restarts only once.
    'Capture:{guid}:MFX' / ':EFX' moves the mic chain to that slot (the app's "try another
    effect slot"): some mic arrays never start an endpoint effect but run mode effects.
  The mic chain sits in ONE slot per microphone; without a slot it stays where it is (EFX first).
  -AppExe (the app's exe, passed by the app): it is installed next to the effects
    (%ProgramFiles%\GamersEqualizer\gamersequalizer.exe; the app then continues from there and
    "start with Windows" points there), unless it runs from there already or -NoAppInstall (a
    developer's build). Afterwards it opens the microphone for a few seconds to check that
    Windows starts the mic chain; if not, the chain moves to the other slot (EFX <-> MFX) and
    is checked once more.

.DESCRIPTION
  - Backs up each endpoint's FxProperties key (reg export) to %ProgramFiles%\GamersEqualizer\apo-backup.
  - Copies target\apo\gamersequalizer_apo.dll to %ProgramFiles%\GamersEqualizer\apo.
  - Registers the COM classes and the APOs. Settings come from the running app (shared memory).
  - APPENDS our CLSIDs to each endpoint's composite effect lists (EFX / SFX). Existing
    Realtek/Microsoft effects stay in place and run before ours.
  - Restarts the Windows Audio service once (all audio stops for a few seconds).

  Undo: scripts\apo-uninstall.ps1 (optionally -RestoreBackup <file.reg>).

.EXAMPLE
  .\apo-install.ps1                       # the only active microphone
  .\apo-install.ps1 -EndpointId '{cf0a2270-1885-4dd6-82a5-f636e9efadd5}'
  .\apo-install.ps1 -Flow Render -EndpointId '{b45cef56-a56b-49b0-88c0-4af057026718}'
  .\apo-install.ps1 -Flow Render -Slot SFX -EndpointId '{b45cef56-a56b-49b0-88c0-4af057026718}'
  .\apo-install.ps1 -Endpoint 'Capture:{cf0a2270-1885-4dd6-82a5-f636e9efadd5}','Render:{b45cef56-a56b-49b0-88c0-4af057026718}'
#>
param(
  [string]$EndpointId,
  # Several endpoints, each 'Capture:{guid}' or 'Render:{guid}'. Overrides -Flow / -EndpointId.
  [string[]]$Endpoint,
  # DLL to install; default: the dev build in target\apo.
  [string]$SourceDll,
  # Started by the app's "fix" button: keep the window open on errors, briefly on success.
  [switch]$FromApp,
  # Default: render EFX; capture keeps the slot it is in (EFX for a new microphone).
  [ValidateSet('EFX', 'MFX', 'SFX')][string]$Slot,
  [ValidateSet('Capture', 'Render')][string]$Flow = 'Capture',
  # Render: skip the stream EQ (chat / media channels).
  [switch]$NoStreamEq,
  # The app's exe: installed next to the effects, and `--probe-mic {guid}` opens the microphone
  # for the self-test after installing.
  [string]$AppExe,
  # SHA-256 (hex) the app computed: files from its user-writable folder are used only when the
  # copy this elevated script makes hashes the same.
  [string]$AppExeSha256,
  [string]$SourceDllSha256,
  # A developer's build: it runs in place, only a temporary copy is made for the self-test.
  [switch]$NoAppInstall
)

$ErrorActionPreference = 'Stop'
trap {
  # A trap runs before `finally`; if the user closes this window at the prompt below, the
  # `finally` that restarts audio never runs. Start it here first (no-op when it is running).
  Start-Service Audiosrv -ErrorAction SilentlyContinue -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  if ($FromApp) {
    Say "`nInstall failed: $_" Red
    Read-Host 'Press Enter to close'
    exit 1
  }
  break
}

# A click into this window starts the console's text selection, which pauses the script at its
# next output until a key is pressed. So nothing is written to the window while Windows audio
# is stopped (those lines are held and shown once it runs again): a paused window never leaves
# the PC without sound.
$AudioStopped = $false
$Held = New-Object System.Collections.Generic.List[string]

# One line for the window and for the app's log: the app's service copies Application-log
# entries of this source into it ([ApoNote] ... installer: ...), so the log shows how far an
# install got. The source is registered on the first entry; apo-uninstall.ps1 removes it.
function Say([string]$text, [string]$color) {
  if ($text.Trim()) {
    try { [System.Diagnostics.EventLog]::WriteEntry('GamersEqualizer', "installer: $($text.Trim())") } catch { }
  }
  if ($script:AudioStopped) { $Held.Add($text) }
  elseif ($color) { Write-Host $text -ForegroundColor $color }
  else { Write-Host $text }
}

function Stop-Audio {
  $script:AudioStopped = $true
  Stop-Service Audiosrv -Force -WarningAction SilentlyContinue
}

# Returns when audio started (for Test-MicChain), then shows the held lines.
function Start-Audio {
  $started = Get-Date
  Start-Service Audiosrv -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  foreach ($line in $Held) { Write-Host $line }
  $Held.Clear()
  $started
}

# Must match CLSID_GAIN_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
# crates/gamersequalizer-apo/src/lib.rs. All classes live in the same DLL.
$Classes = @{
  Capture = @{ Clsid = '{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}'; Name = 'GamersEqualizer Mic Chain' }
  Render  = @{ Clsid = '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}'; Name = 'GamersEqualizer Output EQ' }
  Stream  = @{ Clsid = '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}'; Name = 'GamersEqualizer Stream EQ' }
}
$DeviceWords = @{ Capture = 'microphone'; Render = 'output device' }
$ApoInterfaces = @(
  '{fd7f2b29-24d0-4b5c-b177-592c39f9ca10}', # IAudioProcessingObject
  '{9e1d6a6d-ddbc-4e95-a4c7-ad64ba37846c}', # IAudioProcessingObjectRT
  '{0e5ed805-aba6-49c3-8f9a-2b8c889c4fa8}', # IAudioProcessingObjectConfiguration
  '{5fa00f27-add6-499a-8a9d-6b98521fa75b}'  # IAudioSystemEffects
)

# Built with the unwinding `apo` profile (root Cargo.toml), not `release`.
if (-not $SourceDll) { $SourceDll = Join-Path $PSScriptRoot '..\target\apo\gamersequalizer_apo.dll' }
# Known-folder lookup, not $env:ProgramFiles: the user can override that variable in
# HKCU\Environment and this elevated process inherits it (DLL for audiodg in a user folder).
# Backups live here too: under ProgramData any user can pre-create our folder (or a junction).
$ProgramDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer'
$InstallDir = Join-Path $ProgramDir 'apo'
$InstalledDll = Join-Path $InstallDir 'gamersequalizer_apo.dll'
$InstalledExe = Join-Path $ProgramDir 'gamersequalizer.exe'
$BackupDir = Join-Path $ProgramDir 'apo-backup'

$AudioRoot = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$FxGuid = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
# Composite (Windows 11, REG_MULTI_SZ) and legacy (REG_SZ) value names per slot.
$CompositeValues = @{ SFX = "$FxGuid,13"; MFX = "$FxGuid,14"; EFX = "$FxGuid,15" }
# Legacy SFX may also sit in the pre-Windows 8.1 pre-mix (LFX) value ,1.
$LegacyValues = @{ SFX = @("$FxGuid,5", "$FxGuid,1"); MFX = @("$FxGuid,6"); EFX = @("$FxGuid,7") }
$NameProp = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
$DeviceDescProp = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$DisableSysFxProp = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'

# FxProperties grants Administrators only SetValue + read; the registry provider (New-ItemProperty)
# asks for more and gets "access denied", so open the key with exactly these rights.
function Set-FxValue([string]$subKey, [string]$name, [string[]]$value) {
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    if ($value.Count -eq 0) { $key.DeleteValue($name, $false) }
    else { $key.SetValue($name, $value, [Microsoft.Win32.RegistryValueKind]::MultiString) }
  }
  finally { $key.Dispose() }
}

function Get-ActiveEndpoints([string]$flow) {
  Get-ChildItem (Join-Path $AudioRoot $flow) | Where-Object { (Get-ItemProperty $_.PSPath).DeviceState -eq 1 } | ForEach-Object {
    $props = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
    [pscustomobject]@{ Id = $_.PSChildName; Name = "$($props.$NameProp) ($($props.$DeviceDescProp))" }
  }
}

# Appends $class to the endpoint's composite list for $slot, seeded from the legacy single value
# so that effect is kept. The mic chain also leaves the other capture slot: in both, it could run twice.
function Set-EffectSlot($job, [string]$class, [string]$slot) {
  $clsid = $Classes[$class].Clsid
  $value = $CompositeValues[$slot]
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $list = @()
  if ($null -ne $fx.$value) { $list = @($fx.$value) }
  else {
    $legacy = $LegacyValues[$slot] | ForEach-Object { $fx.$_ } |
      Where-Object { $null -ne $_ -and $_ -ne '{00000000-0000-0000-0000-000000000000}' } | Select-Object -First 1
    if ($legacy) { $list = @($legacy) }
  }
  if (-not ($list | Where-Object { $_ -ieq $clsid })) { $list += $clsid }
  $fxSubKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
  Set-FxValue $fxSubKey $value ([string[]]$list)
  Say "$($job.Name) $slot chain: $($list -join ' -> ')"
  if ($class -ne 'Capture') { return }
  $otherSlot = if ($slot -eq 'MFX') { 'EFX' } else { 'MFX' }
  $otherValue = $CompositeValues[$otherSlot]
  $before = @($fx.$otherValue | Where-Object { $_ })
  $rest = @($before | Where-Object { $_ -ine $clsid })
  if ($rest.Count -ne $before.Count) {
    Set-FxValue $fxSubKey $otherValue ([string[]]$rest)
    Say "$($job.Name): removed from the $otherSlot chain"
  }
}

# The exe that opens the microphone for the self-test, or nothing (check skipped). Elevated, so
# never the exe in the user's folder: the installed app when it is the build the app hashed
# (installed just now, or the app runs from there), else a temporary copy made where only
# administrators write, used when it hashes as the app said.
# shortcut: the app hashes its exe file when it starts this script; same-user malware that had
# swapped that file before passes. Upgrade: sign the exe and check the signature here.
function Get-ProbeExe {
  if (-not ($AppExe -and $AppExeSha256 -and (Test-Path $AppExe))) {
    Say "`nMicrophone check skipped: the app did not pass its exe."
    return
  }
  # The install is done: a failure here only skips the check, it is not "Install failed".
  try {
    if ((Test-Path $InstalledExe) -and (Get-FileHash $InstalledExe -Algorithm SHA256).Hash -eq $AppExeSha256) {
      return $InstalledExe
    }
    Copy-Item $AppExe $ProbeTemp -Force
    if ((Get-FileHash $ProbeTemp -Algorithm SHA256).Hash -ne $AppExeSha256) {
      Remove-Item $ProbeTemp -Force
      Say "`nMicrophone check skipped: the app's exe changed after the app started this installer." Yellow
      return
    }
  }
  catch {
    Say "`nMicrophone check skipped: $_" Yellow
    return
  }
  $ProbeTemp
}

# Audio enhancements off for the device (flag in either key, as the app reads it): Windows runs
# no effect on it at all.
function Test-EnhancementsOff($job) {
  [bool](@('FxProperties', 'Properties') | Where-Object {
      (Get-ItemProperty (Join-Path $job.Key $_) -ErrorAction SilentlyContinue).$DisableSysFxProp -eq 1 })
}

# 'started' when Windows started the mic chain for a stream since $since (LockForProcess notes
# "Mic Chain: stream ..." in the Application log), 'not started', or $null when the microphone
# could not be opened (busy, blocked, an error): then nothing is known and nothing moves.
# shortcut: a note does not name its microphone, so one from another mic with our chain counts
# too (errs towards "works"). Upgrade: put the endpoint id into the APO's notes.
function Test-MicChain($job, [datetime]$since) {
  # A just restarted audio service may not list the device yet.
  Start-Sleep -Seconds 2
  try {
    # -NoNewWindow starts it directly (no shell, so no SmartScreen prompt for the unsigned exe).
    $probe = Start-Process -FilePath $ProbeCopy -ArgumentList '--probe-mic', $job.Id -NoNewWindow -PassThru
    # Windows PowerShell loses ExitCode unless the handle is opened before the process ends.
    $null = $probe.Handle
    # It reads the mic for 3 s; a hung one must not block the installer.
    if (-not $probe.WaitForExit(20000)) {
      $probe.Kill()
      Say '  The check did not finish in time.'
      return $null
    }
  }
  catch {
    Say "  The check could not start: $_"
    return $null
  }
  if ($probe.ExitCode -ne 0) {
    Say '  The microphone could not be opened (busy or blocked; details in the app log).'
    return $null
  }
  $iso = $since.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", [Globalization.CultureInfo]::InvariantCulture)
  $xpath = "*[System[Provider[@Name='GamersEqualizer'] and TimeCreated[@SystemTime>='$iso']]]"
  $notes = @(Get-WinEvent -LogName Application -FilterXPath $xpath -MaxEvents 200 -ErrorAction SilentlyContinue)
  if ($notes | Where-Object { "$($_.Properties[0].Value)" -match 'GamersEqualizer Mic Chain: (stream |refused a stream)' }) {
    return 'started'
  }
  'not started'
}

# Puts the mic chain into $slot (out of the other one), with audio stopped; returns when audio
# started again, for Test-MicChain.
function Move-MicChain($job, [string]$slot) {
  Stop-Audio
  try { Set-EffectSlot $job 'Capture' $slot }
  finally { $started = Start-Audio }
  $started
}

# Capture keeps the slot it was moved to, so an update does not undo the move.
function Get-DefaultSlot([string]$flow, [string]$endpointKey) {
  if ($flow -ne 'Capture') { return 'EFX' }
  $mfx = (Get-ItemProperty (Join-Path $endpointKey 'FxProperties')).($CompositeValues.MFX)
  if (@($mfx) -contains $Classes.Capture.Clsid) { return 'MFX' }
  'EFX'
}

# (class, slot) pairs for one endpoint.
function Get-Targets([string]$flow, [string]$slot, [bool]$noStreamEq) {
  if ($flow -eq 'Capture') {
    if ($slot -eq 'SFX') { throw 'The SFX slot is only used on render endpoints (-Flow Render).' }
    return , @(, @('Capture', $slot))
  }
  if ($slot -eq 'SFX') { return , @(, @('Stream', 'SFX')) }
  $targets = @(, @('Render', $slot))
  if (-not $noStreamEq) { $targets += , @('Stream', 'SFX') }
  return , $targets
}

# Checks one endpoint before anything changes; returns the job to install.
function Resolve-Job([string]$flow, [string]$endpointId, [string]$slot, [bool]$noStreamEq) {
  $word = $DeviceWords[$flow]
  $active = @(Get-ActiveEndpoints $flow)
  if (-not $endpointId) {
    if ($active.Count -ne 1) {
      $active | Format-Table -AutoSize | Out-String | Write-Host
      throw "Found $($active.Count) active $($word)s; pass -EndpointId with one of the ids above."
    }
    $endpointId = $active[0].Id
  }
  $ep = $active | Where-Object { $_.Id -ieq $endpointId }
  if (-not $ep) { throw "Endpoint $endpointId is not an active $flow device." }
  $endpointKey = Join-Path (Join-Path $AudioRoot $flow) $endpointId
  if (-not (Test-Path (Join-Path $endpointKey 'FxProperties'))) {
    throw "$($ep.Name) has no FxProperties key (no effects configured); not supported by this prototype."
  }
  if (-not $slot) { $slot = Get-DefaultSlot $flow $endpointKey }
  if ($CompositeValues.Keys -notcontains $slot) { throw "No effect slot for $($ep.Name) ('$slot'); nothing changed." }
  [pscustomobject]@{
    Flow = $flow; Id = $endpointId; Name = $ep.Name; Word = $word; Key = $endpointKey
    Targets = Get-Targets $flow $slot $noStreamEq
  }
}

# --- Check every endpoint first: an error leaves the system untouched ----------------------
$jobs = @()
if ($Endpoint) {
  # `powershell -File ... -Endpoint a,b` (how the app starts this) passes ONE string "a,b";
  # only `-Command` or a call from PowerShell makes it an array. Split either way.
  foreach ($spec in ($Endpoint -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    $flowName, $id, $specSlot = $spec -split ':', 3
    if ($DeviceWords.Keys -notcontains $flowName -or -not $id) { throw "Bad -Endpoint '$spec' (expected Capture:{guid} or Render:{guid})." }
    if ($specSlot -and ($flowName -ne 'Capture' -or @('EFX', 'MFX') -notcontains $specSlot)) {
      throw "Bad -Endpoint '$spec' (a slot is only Capture:{guid}:EFX or Capture:{guid}:MFX)."
    }
    $jobs += Resolve-Job $flowName $id $specSlot $false
  }
}
else {
  $jobs += Resolve-Job $Flow $EndpointId $Slot $NoStreamEq.IsPresent
}
if (-not (Test-Path $SourceDll)) {
  throw "DLL not found: $SourceDll. Build it first: cargo build --profile apo -p gamersequalizer-apo"
}
Say 'Do not click inside this window: a click pauses it until you press Enter.'
foreach ($job in $jobs) {
  $what = ($job.Targets | ForEach-Object { "$($Classes[$_[0]].Name) ($($_[1]))" }) -join ', '
  Say "Target: $($job.Name) $($job.Id)`: $what"
}

# --- Backup -----------------------------------------------------------------------------
New-Item -ItemType Directory -Force $BackupDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
foreach ($job in $jobs) {
  $backupFile = Join-Path $BackupDir "$($job.Id)-FxProperties-$stamp.reg"
  $regPath = "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
  & reg.exe export $regPath $backupFile /y | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Backup failed (reg export exit $LASTEXITCODE); nothing changed." }
  Say "Backup: $backupFile"
}

# --- The app itself: staged and checked before anything changes ---------------------------
# It moves next to its effects, unless it runs from there already or is a developer's build.
$installApp = $false
if ($AppExe -and $AppExeSha256 -and -not $NoAppInstall -and (Test-Path $AppExe)) {
  $installApp = [IO.Path]::GetFullPath($AppExe) -ine $InstalledExe
}
$stagedExe = "$InstalledExe.new"
if ($installApp) {
  Copy-Item $AppExe $stagedExe -Force
  if ((Get-FileHash $stagedExe -Algorithm SHA256).Hash -ne $AppExeSha256) {
    Remove-Item $stagedExe -Force
    throw 'The app exe changed after the app checked it; nothing changed.'
  }
}

# --- Install (audio service stopped once, so audiodg releases an older DLL) ---------------
Stop-Audio
try {
  New-Item -ItemType Directory -Force $InstallDir | Out-Null
  # Staged and checked first: the source sits in a user-writable folder, audiodg loads the result.
  $stagedDll = "$InstalledDll.new"
  Copy-Item $SourceDll $stagedDll -Force
  if ($SourceDllSha256 -and (Get-FileHash $stagedDll -Algorithm SHA256).Hash -ne $SourceDllSha256) {
    Remove-Item $stagedDll -Force
    throw 'The DLL next to the app changed after the app checked it; nothing installed.'
  }
  Move-Item $stagedDll $InstalledDll -Force
  # A DLL from a downloaded ZIP carries the internet zone mark; drop it from the installed copy.
  Unblock-File $InstalledDll

  foreach ($job in $jobs) {
    foreach ($target in $job.Targets) {
      $Clsid = $Classes[$target[0]].Clsid
      $FriendlyName = $Classes[$target[0]].Name
      $TargetSlot = $target[1]

      $clsidKey = "HKLM:\SOFTWARE\Classes\CLSID\$Clsid"
      New-Item -Force "$clsidKey\InprocServer32" | Out-Null
      Set-Item $clsidKey -Value $FriendlyName
      Set-Item "$clsidKey\InprocServer32" -Value $InstalledDll
      New-ItemProperty "$clsidKey\InprocServer32" -Name ThreadingModel -Value 'Both' -PropertyType String -Force | Out-Null

      $apoKey = "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$Clsid"
      New-Item -Force $apoKey | Out-Null
      $dwords = @{
        MajorVersion = 1; MinorVersion = 0; Flags = 15
        MinInputConnections = 1; MaxInputConnections = 1; MinOutputConnections = 1; MaxOutputConnections = 1
        MaxInstances = -1; NumAPOInterfaces = $ApoInterfaces.Count
      }
      foreach ($k in $dwords.Keys) { New-ItemProperty $apoKey -Name $k -Value $dwords[$k] -PropertyType DWord -Force | Out-Null }
      New-ItemProperty $apoKey -Name FriendlyName -Value $FriendlyName -PropertyType String -Force | Out-Null
      New-ItemProperty $apoKey -Name Copyright -Value 'GamersEqualizer' -PropertyType String -Force | Out-Null
      for ($i = 0; $i -lt $ApoInterfaces.Count; $i++) {
        New-ItemProperty $apoKey -Name "APOInterface$i" -Value $ApoInterfaces[$i] -PropertyType String -Force | Out-Null
      }

      Set-EffectSlot $job $target[0] $TargetSlot
    }
  }

  # Last, so the app that started this finds everything else in place when it sees its
  # installed copy and continues from there.
  if ($installApp) {
    # An older installed copy (its service, its window) holds the file open.
    Get-Process gamersequalizer -ErrorAction SilentlyContinue | Where-Object { $_.Path -ieq $InstalledExe } | Stop-Process -Force
    for ($try = 1; ; $try++) {
      try { Move-Item $stagedExe $InstalledExe -Force; break }
      catch {
        if ($try -ge 10) { throw "The installed app could not be replaced ($InstalledExe): $_" }
        Start-Sleep -Milliseconds 500
      }
    }
    # The same bytes the user started and approved (hash checked above), now installed: without
    # the internet zone mark of the downloaded ZIP, like the DLL.
    Unblock-File $InstalledExe
    Say "App: $InstalledExe"
  }
}
finally {
  $audioStarted = Start-Audio
}

foreach ($job in $jobs) {
  if (Test-EnhancementsOff $job) {
    Write-Warning "Audio enhancements are OFF for $($job.Name); turn them on (Settings > Sound > device > Audio enhancements), or the APO is not loaded."
  }
}

# --- Self-test: does Windows start the mic chain where it was put? ------------------------
# Some mic arrays never start an endpoint effect (EFX) but run mode effects (MFX), or the
# other way round, and nothing in the registry tells which in advance.
$selfTestFailed = $false
$ProbeTemp = Join-Path $InstallDir 'gamersequalizer-probe.exe'
$ProbeCopy = $null
$micJobs = @($jobs | Where-Object { $_.Flow -eq 'Capture' })
if ($micJobs.Count -gt 0) { $ProbeCopy = Get-ProbeExe }
if (-not $ProbeCopy) { $micJobs = @() }
foreach ($job in $micJobs) {
  # Enhancements off: Windows runs no effect in either slot; the warning above says so.
  if (Test-EnhancementsOff $job) { continue }
  $slot = $job.Targets[0][1]
  Say "`nChecking $($job.Name): opening it for a few seconds..."
  $result = Test-MicChain $job $audioStarted
  if ($result -eq 'started') { Say "  Works: Windows starts the mic effect ($slot)." Green }
  if ($result -ne 'not started') { continue }
  $other = if ($slot -eq 'MFX') { 'EFX' } else { 'MFX' }
  Say "  Windows did not start it in $slot; moving it to $other (audio restarts once more)..."
  $audioStarted = Move-MicChain $job $other
  $result = Test-MicChain $job $audioStarted
  if ($result -eq 'started') { Say "  Works: Windows starts the mic effect ($other)." Green }
  # Unknown in $other, but $slot is known not to work: it stays in $other.
  if ($result -ne 'not started') { continue }
  # Neither works (or the check cannot see it): back where it was put, the default or the user's choice.
  Say "  Windows starts the mic effect neither in EFX nor in MFX: apps get the raw microphone." Red
  Say "  Putting it back in $slot. Send the app log (%APPDATA%\GamersEqualizer\logs\gamersequalizer.log)." Red
  $audioStarted = Move-MicChain $job $slot
  $selfTestFailed = $true
}
if (Test-Path $ProbeTemp) { Remove-Item $ProbeTemp -Force -ErrorAction SilentlyContinue }

Say ''
Say 'Installed. Apps that had a microphone or output open lost it when the audio service restarted: reopen them.'
if ($installApp) { Say "The app now runs from $InstalledExe; the folder it was started from can be deleted." }
Say 'Settings come from the GamersEqualizer app; keep it running.'
if ($FromApp -and $selfTestFailed) { Read-Host 'Press Enter to close' }
elseif ($FromApp) { Start-Sleep -Seconds 3 }
