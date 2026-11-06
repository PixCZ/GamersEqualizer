#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Installs a GamersEqualizer APO on one physical endpoint:
  -Flow Capture (default): the mic chain (AEC, noise suppression, EQ, boost) on a microphone.
  -Flow Render: the output EQ (headphone correction + game profile, EFX) plus the stream EQ
    (chat / media profiles per app stream, SFX) on headphones/speakers.
  -Flow Render -Slot SFX: the stream EQ only.  -NoStreamEq: the output EQ only.

.DESCRIPTION
  - Backs up the endpoint's FxProperties key (reg export) to %ProgramData%\GamersEqualizer\apo-backup.
  - Copies target\apo\gamersequalizer_apo.dll to %ProgramFiles%\GamersEqualizer\apo.
  - Registers the COM class and the APO. Settings come from the running app (shared memory).
  - APPENDS our CLSIDs to the endpoint's composite effect lists (EFX / SFX). Existing
    Realtek/Microsoft effects stay in place and run before ours.
  - Restarts the Windows Audio service (all audio stops for a few seconds).

  Undo: scripts\apo-uninstall.ps1 (optionally -RestoreBackup <file.reg>).

.EXAMPLE
  .\apo-install.ps1                       # the only active microphone
  .\apo-install.ps1 -EndpointId '{cf0a2270-1885-4dd6-82a5-f636e9efadd5}'
  .\apo-install.ps1 -Flow Render -EndpointId '{b45cef56-a56b-49b0-88c0-4af057026718}'
  .\apo-install.ps1 -Flow Render -Slot SFX -EndpointId '{b45cef56-a56b-49b0-88c0-4af057026718}'
#>
param(
  [string]$EndpointId,
  # DLL to install; default: the dev build in target\apo.
  [string]$SourceDll,
  # Started by the app's "fix" button: keep the window open on errors, briefly on success.
  [switch]$FromApp,
  [ValidateSet('EFX', 'MFX', 'SFX')][string]$Slot = 'EFX',
  [ValidateSet('Capture', 'Render')][string]$Flow = 'Capture',
  # Render: skip the stream EQ (chat / media channels).
  [switch]$NoStreamEq
)

$ErrorActionPreference = 'Stop'
trap {
  if ($FromApp) {
    Write-Host "`nInstall failed: $_" -ForegroundColor Red
    Read-Host 'Press Enter to close'
    exit 1
  }
  break
}

# Must match CLSID_GAIN_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
# crates/gamersequalizer-apo/src/lib.rs. All classes live in the same DLL.
if ($Flow -eq 'Capture' -and $Slot -eq 'SFX') { throw 'The SFX slot is only used on render endpoints (-Flow Render).' }
$Classes = @{
  Capture = @{ Clsid = '{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}'; Name = 'GamersEqualizer Mic Chain' }
  Render  = @{ Clsid = '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}'; Name = 'GamersEqualizer Output EQ' }
  Stream  = @{ Clsid = '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}'; Name = 'GamersEqualizer Stream EQ' }
}
# (class, slot) pairs to install on this endpoint.
$Targets = @()
if ($Flow -eq 'Capture') { $Targets += , @('Capture', $Slot) }
elseif ($Slot -eq 'SFX') { $Targets += , @('Stream', 'SFX') }
else {
  $Targets += , @('Render', $Slot)
  if (-not $NoStreamEq) { $Targets += , @('Stream', 'SFX') }
}
$DeviceWord = @{ Capture = 'microphone'; Render = 'output device' }[$Flow]
$ApoInterfaces = @(
  '{fd7f2b29-24d0-4b5c-b177-592c39f9ca10}', # IAudioProcessingObject
  '{9e1d6a6d-ddbc-4e95-a4c7-ad64ba37846c}', # IAudioProcessingObjectRT
  '{0e5ed805-aba6-49c3-8f9a-2b8c889c4fa8}', # IAudioProcessingObjectConfiguration
  '{5fa00f27-add6-499a-8a9d-6b98521fa75b}'  # IAudioSystemEffects
)

# Built with the unwinding `apo` profile (root Cargo.toml), not `release`.
if (-not $SourceDll) { $SourceDll = Join-Path $PSScriptRoot '..\target\apo\gamersequalizer_apo.dll' }
$InstallDir = Join-Path $env:ProgramFiles 'GamersEqualizer\apo'
$InstalledDll = Join-Path $InstallDir 'gamersequalizer_apo.dll'
$BackupDir = Join-Path $env:ProgramData 'GamersEqualizer\apo-backup'

$FlowRoot = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$Flow"
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

function Get-ActiveEndpoints {
  Get-ChildItem $FlowRoot | Where-Object { (Get-ItemProperty $_.PSPath).DeviceState -eq 1 } | ForEach-Object {
    $props = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
    [pscustomobject]@{ Id = $_.PSChildName; Name = "$($props.$NameProp) ($($props.$DeviceDescProp))" }
  }
}

# --- Pick the endpoint ------------------------------------------------------------------
$active = @(Get-ActiveEndpoints)
if (-not $EndpointId) {
  if ($active.Count -ne 1) {
    $active | Format-Table -AutoSize | Out-String | Write-Host
    throw "Found $($active.Count) active $($DeviceWord)s; pass -EndpointId with one of the ids above."
  }
  $EndpointId = $active[0].Id
}
$endpoint = $active | Where-Object { $_.Id -ieq $EndpointId }
if (-not $endpoint) { throw "Endpoint $EndpointId is not an active $Flow device." }

$endpointKey = Join-Path $FlowRoot $EndpointId
$fxKey = Join-Path $endpointKey 'FxProperties'
if (-not (Test-Path $fxKey)) {
  throw "Endpoint has no FxProperties key (no effects configured); not supported by this prototype."
}
if (-not (Test-Path $SourceDll)) {
  throw "DLL not found: $SourceDll. Build it first: cargo build --profile apo -p gamersequalizer-apo"
}
$what = ($Targets | ForEach-Object { "$($Classes[$_[0]].Name) ($($_[1]))" }) -join ', '
Write-Host "Target: $($endpoint.Name) $EndpointId`: $what"

# --- Backup -----------------------------------------------------------------------------
New-Item -ItemType Directory -Force $BackupDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backupFile = Join-Path $BackupDir "$EndpointId-FxProperties-$stamp.reg"
$regPath = "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$Flow\$EndpointId\FxProperties"
& reg.exe export $regPath $backupFile /y | Out-Null
if ($LASTEXITCODE -ne 0) { throw "Backup failed (reg export exit $LASTEXITCODE); nothing changed." }
Write-Host "Backup: $backupFile"

# --- Install (audio service stopped so audiodg releases an older DLL) ---------------------
Stop-Service Audiosrv -Force
try {
  New-Item -ItemType Directory -Force $InstallDir | Out-Null
  Copy-Item $SourceDll $InstalledDll -Force

  foreach ($target in $Targets) {
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

    # Append to the composite list; seed it from the legacy single value so that effect is kept.
    $CompositeValue = $CompositeValues[$TargetSlot]
    $fx = Get-ItemProperty $fxKey
    $list = @()
    if ($null -ne $fx.$CompositeValue) { $list = @($fx.$CompositeValue) }
    else {
      $legacy = $LegacyValues[$TargetSlot] | ForEach-Object { $fx.$_ } |
        Where-Object { $null -ne $_ -and $_ -ne '{00000000-0000-0000-0000-000000000000}' } | Select-Object -First 1
      if ($legacy) { $list = @($legacy) }
    }
    if (-not ($list | Where-Object { $_ -ieq $Clsid })) { $list += $Clsid }
    Set-FxValue "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$Flow\$EndpointId\FxProperties" $CompositeValue ([string[]]$list)
    Write-Host "$TargetSlot chain: $($list -join ' -> ')"
  }
}
finally {
  Start-Service Audiosrv
}

$sysFx = (Get-ItemProperty (Join-Path $endpointKey 'Properties')).$DisableSysFxProp
if ($sysFx -eq 1) {
  Write-Warning "Audio enhancements are OFF for this $DeviceWord; turn them on (Settings > Sound > device > Audio enhancements), or the APO is not loaded."
}

Write-Host ''
Write-Host "Installed. Apps that had the $DeviceWord open lost it when the audio service restarted: reopen them."
Write-Host 'Settings come from the GamersEqualizer app; keep it running.'
if ($FromApp) { Start-Sleep -Seconds 3 }
