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
    developer's build). Afterwards it probes every installed Capture/Render endpoint for a few
    seconds: ensures Windows starts the effects without audiodg crashes, tries MFX fallback if
    EFX does not start, and rolls back automatically on open failure or crash.

.DESCRIPTION
  - Takes an in-memory snapshot of each endpoint's FxProperties values before changing anything
    (no persistent backup folder on disk) so self-test failures roll back cleanly in memory.
  - Copies target\apo\gamersequalizer_apo.dll to %ProgramFiles%\GamersEqualizer\apo.
  - Registers the COM classes and the APOs. Settings come from the running app (shared memory).
  - Promotes any legacy single-CLSID values (,1 / ,5 / ,6 / ,7) into composite lists (,13 / ,14 / ,15)
    without dropping existing OEM effects, ensures the default processing mode is registered on each
    configured slot, and appends our CLSIDs.
  - Restarts the Windows Audio service once (all audio stops for a few seconds).

  Undo: scripts\apo-uninstall.ps1.
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
  # The app's exe: installed next to the effects, and `--probe-mic` / `--probe-render` test endpoints
  # after installing.
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
    $loc = if ($_.InvocationInfo -and $_.InvocationInfo.ScriptLineNumber) {
      " (line $($_.InvocationInfo.ScriptLineNumber): $($_.InvocationInfo.Line.Trim()))"
    }
    else { '' }
    Say "`nInstall failed: $_$loc" Red
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

# Returns when audio started (for probe / crash checks), then shows the held lines.
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
$ProgramDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer'
$InstallDir = Join-Path $ProgramDir 'apo'
$InstalledDll = Join-Path $InstallDir 'gamersequalizer_apo.dll'
$InstalledExe = Join-Path $ProgramDir 'gamersequalizer.exe'

$AudioRoot = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$FxGuid = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$NullGuid = '{00000000-0000-0000-0000-000000000000}'
# Fixed Microsoft Windows SDK PKEY_FX composite (Win10/11 REG_MULTI_SZ) and legacy (REG_SZ) slots.
$CompositeValues = @{ SFX = "$FxGuid,13"; MFX = "$FxGuid,14"; EFX = "$FxGuid,15" }
# Legacy SFX may also sit in the pre-Windows 8.1 pre-mix (LFX) value ,1.
$LegacyValues = @{ SFX = @("$FxGuid,5", "$FxGuid,1"); MFX = @("$FxGuid,6"); EFX = @("$FxGuid,7") }
# Fixed Microsoft Windows SDK PKEY_SFX/MFX/EFX_ProcessingModes_Supported_For_Streaming + DEFAULT mode.
$ModesGuid = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$DefaultProcessingMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'
$ModeValues = @{ SFX = "$ModesGuid,5"; MFX = "$ModesGuid,6"; EFX = "$ModesGuid,7" }

$NameProp = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
$DeviceDescProp = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
$DisableSysFxProp = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'

# FxProperties grants Administrators only SetValue + QueryValues (ReadKey); subkeys (Volatile /
# Default / User) are owned by TrustedInstaller / Audiosrv. Open only with these exact rights.
function Set-FxValue([string]$subKey, [string]$name, [string[]]$value) {
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    [string[]]$clean = @($value | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) })
    if ($clean.Count -eq 0) { $key.DeleteValue($name, $false) }
    else { $key.SetValue($name, $clean, [Microsoft.Win32.RegistryValueKind]::MultiString) }
  }
  finally { $key.Dispose() }
}

function Read-FxList($rawValue) {
  $res = New-Object System.Collections.Generic.List[string]
  foreach ($item in @($rawValue)) {
    $s = [string]$item
    if ([string]::IsNullOrWhiteSpace($s)) { continue }
    $trimmed = $s.Trim()
    if ($trimmed -and -not ($res | Where-Object { $_ -ieq $trimmed })) { $res.Add($trimmed) }
  }
  return , $res
}

# Pre-flight ACL check + in-memory snapshot (Administrators have SetValue + QueryValues, not CreateSubKey).
function Get-FxSnapshot([string]$flow, [string]$id) {
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    $records = New-Object System.Collections.Generic.List[object]
    foreach ($vn in ($key.GetValueNames() | Sort-Object)) {
      $val = $key.GetValue($vn, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      $kind = $key.GetValueKind($vn)
      $records.Add([pscustomobject]@{
        Name  = [string]$vn
        Kind  = $kind
        Value = $val
      })
    }
    [pscustomobject]@{
      Flow    = $flow
      Id      = $id
      Records = @($records.ToArray())
    }
  }
  finally { $key.Dispose() }
}

# Exact value-only restore via SetValue + QueryValues (reg.exe import fails on TrustedInstaller subkeys).
function Restore-FxSnapshot($snapshot) {
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($snapshot.Flow)\$($snapshot.Id)\FxProperties"
  $origRecords = @($snapshot.Records)
  $origNames = @($origRecords | ForEach-Object { [string]$_.Name })
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    foreach ($cur in @($key.GetValueNames())) {
      if ($origNames -notcontains $cur) { $key.DeleteValue($cur, $false) }
    }
    foreach ($rec in $origRecords) {
      $kind = [Microsoft.Win32.RegistryValueKind]$rec.Kind
      $valName = [string]$rec.Name
      switch ($kind) {
        ([Microsoft.Win32.RegistryValueKind]::Binary) {
          [byte[]]$binVal = [byte[]]@()
          if ($null -ne $rec.Value) { $binVal = [byte[]]@($rec.Value) }
          $key.SetValue($valName, $binVal, $kind)
        }
        ([Microsoft.Win32.RegistryValueKind]::MultiString) {
          [string[]]$multiVal = [string[]]@()
          if ($null -ne $rec.Value) { $multiVal = [string[]]@($rec.Value | Where-Object { $null -ne $_ } | ForEach-Object { [string]$_ }) }
          $key.SetValue($valName, $multiVal, $kind)
        }
        ([Microsoft.Win32.RegistryValueKind]::DWord) {
          $key.SetValue($valName, [int]$rec.Value, $kind)
        }
        ([Microsoft.Win32.RegistryValueKind]::QWord) {
          $key.SetValue($valName, [long]$rec.Value, $kind)
        }
        default {
          [string]$strVal = if ($null -eq $rec.Value) { '' } else { [string]$rec.Value }
          $key.SetValue($valName, $strVal, $kind)
        }
      }
    }
  }
  finally { $key.Dispose() }
}

function Ensure-ProcessingMode($job, [string]$fxSubKey, [string]$slot) {
  $modeName = $ModeValues[$slot]
  if (-not $modeName) { return }
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $modes = Read-FxList $fx.$modeName
  if (-not ($modes | Where-Object { $_ -ieq $DefaultProcessingMode })) {
    $modes.Add($DefaultProcessingMode)
    Set-FxValue $fxSubKey $modeName ([string[]]$modes.ToArray())
    Say "$($job.Name) $slot modes: $($modes -join ', ')"
  }
}

# Windows 11 ignores composite lists (,13 / ,14 / ,15) when legacy REG_SZ values (,1 / ,5 / ,6 / ,7)
# coexist on the same endpoint (e.g. Microsoft Audio Home Theater Effects on USB headsets/DACs).
# Promote all non-empty legacy CLSIDs into composite lists + ensure their streaming modes + clear legacy.
function Migrate-LegacySlots($job) {
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $fxSubKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
  foreach ($s in 'SFX', 'MFX', 'EFX') {
    $found = New-Object System.Collections.Generic.List[string]
    $presentNames = New-Object System.Collections.Generic.List[string]
    foreach ($legacyName in $LegacyValues[$s]) {
      $val = $fx.$legacyName
      if ($null -eq $val) { continue }
      $presentNames.Add($legacyName)
      foreach ($entry in (Read-FxList $val)) {
        if ($entry -ne $NullGuid -and -not ($found | Where-Object { $_ -ieq $entry })) {
          $found.Add($entry)
        }
      }
    }
    if ($presentNames.Count -eq 0) { continue }
    $compName = $CompositeValues[$s]
    if ($found.Count -gt 0) {
      $compList = Read-FxList $fx.$compName
      foreach ($clsid in $found) {
        if (-not ($compList | Where-Object { $_ -ieq $clsid })) { $compList.Add($clsid) }
      }
      Set-FxValue $fxSubKey $compName ([string[]]$compList.ToArray())
      Ensure-ProcessingMode $job $fxSubKey $s
      Say "$($job.Name) promoted legacy $($presentNames -join '/') -> $compName`: $($compList -join ' -> ')"
    }
    foreach ($legacyName in $presentNames) {
      Set-FxValue $fxSubKey $legacyName @()
    }
  }
}

function Get-ActiveEndpoints([string]$flow) {
  Get-ChildItem (Join-Path $AudioRoot $flow) | Where-Object { (Get-ItemProperty $_.PSPath).DeviceState -eq 1 } | ForEach-Object {
    $props = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
    [pscustomobject]@{ Id = $_.PSChildName; Name = "$($props.$NameProp) ($($props.$DeviceDescProp))" }
  }
}

# Appends $class to the endpoint's composite list for $slot and ensures its streaming modes entry.
# Note: Migrate-LegacySlots has already moved any legacy REG_SZ values safely. Capture also leaves
# the other slot (EFX <-> MFX), keeping any OEM effects there so it never runs twice.
function Set-EffectSlot($job, [string]$class, [string]$slot, [switch]$RemoveFromOther) {
  $clsid = $Classes[$class].Clsid
  $value = $CompositeValues[$slot]
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $list = Read-FxList $fx.$value
  if (-not ($list | Where-Object { $_ -ieq $clsid })) { $list.Add($clsid) }
  $fxSubKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
  Set-FxValue $fxSubKey $value ([string[]]$list.ToArray())
  Say "$($job.Name) $slot chain: $($list -join ' -> ')"
  Ensure-ProcessingMode $job $fxSubKey $slot
  if ($class -ne 'Capture' -and -not $RemoveFromOther) { return }
  $otherSlot = if ($slot -eq 'MFX') { 'EFX' } else { 'MFX' }
  $otherValue = $CompositeValues[$otherSlot]
  $before = Read-FxList $fx.$otherValue
  $rest = @($before | Where-Object { $_ -ine $clsid })
  if ($rest.Count -ne $before.Count) {
    Set-FxValue $fxSubKey $otherValue ([string[]]$rest)
    Say "$($job.Name): removed from the $otherSlot chain"
  }
}

# The exe that opens endpoints for the self-test, or nothing (check skipped). Elevated, so
# never the exe in the user's folder: the installed app when it is the build the app hashed
# (installed just now, or the app runs from there), else a temporary copy made where only
# administrators write, used when it hashes as the app said.
# shortcut: the app hashes its exe file when it starts this script; same-user malware that had
# swapped that file before passes. Upgrade: sign the exe and check the signature here.
function Get-ProbeExe {
  if (-not ($AppExe -and $AppExeSha256 -and (Test-Path $AppExe))) {
    Say "`nEndpoint check skipped: the app did not pass its exe."
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
      Say "`nEndpoint check skipped: the app's exe changed after the app started this installer." Yellow
      return
    }
  }
  catch {
    Say "`nEndpoint check skipped: $_" Yellow
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

function Get-ApoNotesSince([datetime]$since) {
  try {
    return , @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'GamersEqualizer'; StartTime = $since } -ErrorAction Stop |
      Select-Object -First 200 |
      ForEach-Object { "$($_.Properties[0].Value)" })
  }
  catch { return , @() }
}

function Get-AudiodgCrashesSince([datetime]$since) {
  try {
    return , @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'Application Error'; Id = 1000; StartTime = $since } -ErrorAction Stop |
      Select-Object -First 20 |
      Where-Object { $_.Message -match 'audiodg\.exe' })
  }
  catch { return , @() }
}

function Rollback-Job($job, [datetime]$sinceRestart) {
  $snap = $script:FxSnapshots["$($job.Flow):$($job.Id)"]
  if (-not $snap) {
    Say "  Rollback warning: snapshot not found for $($job.Name)." Yellow
    return $sinceRestart
  }
  Stop-Audio
  try { Restore-FxSnapshot $snap }
  finally { $started = Start-Audio }
  Say "  Rolled back $($job.Name) to its exact pre-install FxProperties." Yellow
  $started
}

# Probes one endpoint via `--probe-mic` or `--probe-render` and checks Application EventLog.
# Wait 2 s after Start-Audio before recording `$probeSince` so audiodg startup checks (Realtek
# format probes etc.) do not count as this endpoint's stream start. Also checks EventID 1000
# crashes of audiodg.exe since `$sinceRestart`.
function Invoke-EndpointProbe($job, [datetime]$sinceRestart) {
  $elapsed = ((Get-Date) - $sinceRestart).TotalSeconds
  if ($elapsed -lt 2.0) { Start-Sleep -Milliseconds ([int]((2.0 - $elapsed) * 1000)) }
  $earlyCrashes = Get-AudiodgCrashesSince $sinceRestart
  if ($earlyCrashes.Count -gt 0) {
    return [pscustomobject]@{ OpenOk = $false; Crashed = $true; Notes = @(); PreNotes = @() }
  }
  $preNotes = Get-ApoNotesSince $sinceRestart
  $probeSince = Get-Date
  $flag = if ($job.Flow -eq 'Capture') { '--probe-mic' } else { '--probe-render' }
  try {
    # -NoNewWindow starts it directly (no shell, so no SmartScreen prompt for the unsigned exe).
    $probe = Start-Process -FilePath $ProbeCopy -ArgumentList $flag, $job.Id -NoNewWindow -PassThru
    # Windows PowerShell loses ExitCode unless the handle is opened before the process ends.
    $null = $probe.Handle
    if (-not $probe.WaitForExit(20000)) {
      $probe.Kill()
      Say "  The $($job.Word) check did not finish in time." Red
      return [pscustomobject]@{ OpenOk = $false; Crashed = $false; Notes = @(); PreNotes = $preNotes }
    }
  }
  catch {
    Say "  The check could not start: $_" Red
    return [pscustomobject]@{ OpenOk = $false; Crashed = $false; Notes = @(); PreNotes = $preNotes }
  }
  $crashes = Get-AudiodgCrashesSince $sinceRestart
  $notes = Get-ApoNotesSince $probeSince
  if ($crashes.Count -gt 0) {
    Say "  audiodg.exe crashed during $($job.Word) test." Red
    return [pscustomobject]@{ OpenOk = $false; Crashed = $true; Notes = $notes; PreNotes = $preNotes }
  }
  if ($probe.ExitCode -ne 0) {
    Say "  The $($job.Word) could not be opened cleanly (exit $($probe.ExitCode); details in the app log)." Red
    return [pscustomobject]@{ OpenOk = $false; Crashed = $false; Notes = $notes; PreNotes = $preNotes }
  }
  [pscustomobject]@{ OpenOk = $true; Crashed = $false; Notes = $notes; PreNotes = $preNotes }
}

# Puts an APO class into $slot (removing from the other of EFX/MFX), restarting Windows Audio once.
function Move-EndpointSlot($job, [string]$class, [string]$slot) {
  Stop-Audio
  try { Set-EffectSlot $job $class $slot -RemoveFromOther }
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

# --- Snapshot (in-memory only, no disk backup) -------------------------------------------
$FxSnapshots = @{}
foreach ($job in $jobs) {
  $FxSnapshots["$($job.Flow):$($job.Id)"] = Get-FxSnapshot $job.Flow $job.Id
}
Say 'Pre-install snapshot captured in memory (no disk backup created).'

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
$installComplete = $false
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
    Migrate-LegacySlots $job
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
  $installComplete = $true
}
catch {
  if (-not $installComplete -and $FxSnapshots) {
    foreach ($snap in $FxSnapshots.Values) {
      try { Restore-FxSnapshot $snap } catch { }
    }
  }
  throw
}
finally {
  $audioStarted = Start-Audio
}

foreach ($job in $jobs) {
  if (Test-EnhancementsOff $job) {
    Write-Warning "Audio enhancements are OFF for $($job.Name); turn them on (Settings > Sound > device > Audio enhancements), or the APO is not loaded."
  }
}

# --- Self-test & safe automatic rollback on open failure or audiodg crash -----------------
$selfTestFailed = $false
$ProbeTemp = Join-Path $InstallDir 'gamersequalizer-probe.exe'
$ProbeCopy = Get-ProbeExe

if ($ProbeCopy) {
  # Immediate startup crash check: if Audiosrv startup already crashed audiodg, undo everything at once.
  Start-Sleep -Seconds 2
  $startupCrashes = Get-AudiodgCrashesSince $audioStarted
  if ($startupCrashes.Count -gt 0) {
    Say "`naudiodg.exe crashed right after restarting audio: rolling back all endpoint changes..." Red
    Stop-Audio
    try {
      foreach ($job in $jobs) {
        $snap = $FxSnapshots["$($job.Flow):$($job.Id)"]
        if ($snap) { Restore-FxSnapshot $snap }
      }
    }
    finally { $audioStarted = Start-Audio }
    $selfTestFailed = $true
    $jobs = @()
  }
}

function Test-ApoNoteStarted($res, [string]$className, [string]$guid, [string]$tailPattern) {
  $clsPat = [regex]::Escape($className)
  $idPat = [regex]::Escape($guid)
  $all = @($res.Notes + $res.PreNotes)
  if (@($all | Where-Object { $_ -match "$clsPat \[$idPat\]: $tailPattern" }).Count -gt 0) { return $true }
  if (@($res.Notes | Where-Object { $_ -match "$clsPat`: $tailPattern" }).Count -gt 0) { return $true }
  if (@($all | Where-Object { $_ -match "$clsPat \[\{[0-9a-f-]+\}\]:" }).Count -eq 0 -and
      @($res.PreNotes | Where-Object { $_ -match "$clsPat`: $tailPattern" }).Count -gt 0) {
    return $true
  }
  $false
}

foreach ($job in ($jobs | Where-Object { $_.Flow -eq 'Capture' })) {
  if (-not $ProbeCopy -or (Test-EnhancementsOff $job)) { continue }
  $slot = $job.Targets[0][1]
  Say "`nChecking $($job.Name): opening microphone for a few seconds..."
  $res = Invoke-EndpointProbe $job $audioStarted
  if (-not $res.OpenOk) {
    $audioStarted = Rollback-Job $job $audioStarted
    $selfTestFailed = $true
    continue
  }
  if (Test-ApoNoteStarted $res 'GamersEqualizer Mic Chain' $job.Id '(stream |refused a stream)') {
    Say "  Works: Windows starts the mic effect ($slot)." Green
    continue
  }
  $other = if ($slot -eq 'MFX') { 'EFX' } else { 'MFX' }
  Say "  Windows did not start it in $slot; moving it to $other (audio restarts once more)..."
  $audioStarted = Move-EndpointSlot $job 'Capture' $other
  $res2 = Invoke-EndpointProbe $job $audioStarted
  if (-not $res2.OpenOk) {
    $audioStarted = Rollback-Job $job $audioStarted
    $selfTestFailed = $true
    continue
  }
  if (Test-ApoNoteStarted $res2 'GamersEqualizer Mic Chain' $job.Id '(stream |refused a stream)') {
    Say "  Works: Windows starts the mic effect ($other)." Green
    continue
  }
  # Both slots opened cleanly with zero crashes, yet Windows started neither: restore target slot
  # (keeps raw mic working cleanly and lets health diagnostics surface [ApoCheck] instead of looping).
  Say "  Windows starts the mic effect neither in EFX nor in MFX: apps get the raw microphone." Red
  Say "  Putting it back in $slot. Send the app log (%APPDATA%\GamersEqualizer\logs\gamersequalizer.log)." Red
  $audioStarted = Move-EndpointSlot $job 'Capture' $slot
  $selfTestFailed = $true
}

foreach ($job in ($jobs | Where-Object { $_.Flow -eq 'Render' })) {
  if (-not $ProbeCopy -or (Test-EnhancementsOff $job)) { continue }
  $hasOut = @($job.Targets | Where-Object { $_[0] -eq 'Render' }).Count -gt 0
  $hasStream = @($job.Targets | Where-Object { $_[0] -eq 'Stream' }).Count -gt 0
  Say "`nChecking $($job.Name): opening silent output for a moment..."
  $res = Invoke-EndpointProbe $job $audioStarted
  if (-not $res.OpenOk) {
    $audioStarted = Rollback-Job $job $audioStarted
    $selfTestFailed = $true
    continue
  }
  $outOk = -not $hasOut -or (Test-ApoNoteStarted $res 'GamersEqualizer Output EQ' $job.Id 'stream .* started')
  $streamOk = -not $hasStream -or (Test-ApoNoteStarted $res 'GamersEqualizer Stream EQ' $job.Id 'stream .* started')
  if ($outOk -and $streamOk) {
    Say '  Works: Windows starts the output effects.' Green
    continue
  }
  $outTarget = $job.Targets | Where-Object { $_[0] -eq 'Render' } | Select-Object -First 1
  if ($hasOut -and -not $outOk -and $outTarget -and $outTarget[1] -eq 'EFX') {
    Say '  Output EQ did not start in EFX; trying MFX (audio restarts once more)...'
    $audioStarted = Move-EndpointSlot $job 'Render' 'MFX'
    $res2 = Invoke-EndpointProbe $job $audioStarted
    if (-not $res2.OpenOk) {
      $audioStarted = Rollback-Job $job $audioStarted
      $selfTestFailed = $true
      continue
    }
    $outOk2 = Test-ApoNoteStarted $res2 'GamersEqualizer Output EQ' $job.Id 'stream .* started'
    $streamOk2 = -not $hasStream -or (Test-ApoNoteStarted $res2 'GamersEqualizer Stream EQ' $job.Id 'stream .* started')
    if ($outOk2 -and $streamOk2) {
      Say '  Works: Windows starts the output effects (Output EQ in MFX).' Green
      continue
    }
    if ($outOk2 -or $streamOk2) {
      Say "  Partial output effect start (Output EQ=$outOk2, Stream EQ=$streamOk2). Check the app log (%APPDATA%\GamersEqualizer\logs\gamersequalizer.log)." Yellow
      $selfTestFailed = $true
      continue
    }
  }
  elseif ($outOk -or $streamOk) {
    Say "  Partial output effect start (Output EQ=$outOk, Stream EQ=$streamOk). Check the app log (%APPDATA%\GamersEqualizer\logs\gamersequalizer.log)." Yellow
    $selfTestFailed = $true
    continue
  }

  Say '  Windows did not start output effects: rolling back so the output stays untouched.' Red
  $audioStarted = Rollback-Job $job $audioStarted
  $selfTestFailed = $true
}

if (Test-Path $ProbeTemp) { Remove-Item $ProbeTemp -Force -ErrorAction SilentlyContinue }

Say ''
if ($selfTestFailed) {
  Say 'Completed with warnings/rollback on one or more endpoints. Send %APPDATA%\GamersEqualizer\logs\gamersequalizer.log if needed.' Yellow
}
else {
  Say 'Installed. Apps that had a microphone or output open lost it when the audio service restarted: reopen them.'
}
if ($installApp) { Say "The app now runs from $InstalledExe; the folder it was started from can be deleted." }
Say 'Settings come from the GamersEqualizer app; keep it running.'
if ($FromApp -and $selfTestFailed) { Read-Host 'Press Enter to close' }
elseif ($FromApp) { Start-Sleep -Seconds 3 }
