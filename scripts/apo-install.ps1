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
  -AppOnly: installs the app (with its DLL, the "Add or remove programs" entry and the Start
    menu shortcut) without touching any endpoint: `gamersequalizer.exe --install` on a PC
    without audio devices; the effects follow from the app later.
  -AppExe (the app's exe, passed by the app): it is installed next to the effects
    (%ProgramFiles%\GamersEqualizer\gamersequalizer.exe; the app then continues from there and
    "start with Windows" points there), unless it runs from there already or -NoAppInstall (a
    developer's build). Afterwards it probes every installed Capture/Render endpoint for a few
    seconds: ensures Windows starts the effects without audiodg crashes, tries MFX fallback if
    EFX does not start, and rolls back automatically on open failure or crash.

.DESCRIPTION
  - Takes a snapshot of each endpoint's FxProperties values before changing anything: in memory for
    the self-test rollback, and the first one per endpoint also as JSON in
    %ProgramFiles%\GamersEqualizer\apo-backup (never overwritten), which apo-uninstall.ps1 restores.
  - Copies target\apo\gamersequalizer_apo.dll to %ProgramFiles%\GamersEqualizer\apo.
  - Registers the COM classes and the APOs. Settings come from the running app (shared memory).
  - Promotes any legacy single-CLSID values (,1 / ,5 / ,6 / ,7) into composite lists (,13 / ,14 / ,15)
    without dropping existing OEM effects, ensures the default processing mode is registered on each
    configured slot, and appends our CLSIDs.
  - An endpoint without an FxProperties key (inbox "High Definition Audio Device", VMs) gets one,
    made with the backup/restore privileges (owner and ACL of existing keys unchanged); its backup
    is marked Created, so rollback and apo-uninstall.ps1 delete the key again.
  - Restarts the Windows Audio service once (all audio stops for a few seconds).
  - Needs Windows 10 version 1803 or newer (composite effect lists): an older Windows stops it before
    anything changes. An endpoint whose driver effects would be lost (Get-EndpointBlocker) is never
    touched: with -Endpoint it is skipped and named, and when none is left nothing changes at all.

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
  [switch]$NoAppInstall,
  # The app's version, for the "Add or remove programs" entry (winget compares it).
  [string]$AppVersion,
  # Only the app and its DLL, no endpoint (see .SYNOPSIS).
  [switch]$AppOnly
)

$ErrorActionPreference = 'Stop'
trap {
  # A trap runs before `finally`; if the user closes this window at the prompt below, the
  # `finally` that restarts audio never runs. Start it here first (no-op when it is running).
  Start-Service Audiosrv -ErrorAction SilentlyContinue -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  # Stopped before the install began: FxProperties keys made for it go again ("nothing changed").
  if ($script:MadeKeysPending) {
    foreach ($made in @($script:FxSnapshots.Values | Where-Object { $_.Created })) {
      try { Restore-FxSnapshot $made } catch { Say "Removing the effect list made for $($made.Flow) $($made.Id) failed: $_" Red }
    }
  }
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
# An FxProperties key was made and the install has not begun: the trap deletes it on an error.
$MadeKeysPending = $false

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

# This script writes composite effect lists only (several effects per slot); Windows reads them
# from Windows 10 version 1803 (build 17134) on. Stop before anything changes on an older one.
# Must match MIN_WINDOWS_BUILD in apo_install.rs.
$MinWindowsBuild = 17134
$WindowsBuild = [int](Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').CurrentBuild
if ($WindowsBuild -lt $MinWindowsBuild) {
  throw "Windows build $WindowsBuild is older than Windows 10 version 1803 (build $MinWindowsBuild): it cannot run several effects per slot. Nothing was changed."
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

# Must match CLSID_MIC_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
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
# Must match $BackupDir in apo-uninstall.ps1.
$BackupDir = Join-Path $ProgramDir 'apo-backup'
# Must match apo-uninstall.ps1, which removes both. winget finds the installed app by this key.
$UninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\GamersEqualizer'
$StartMenuLink = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'GamersEqualizer.lnk'

# "Add or remove programs" entry and Start menu shortcut of the installed app. Not fatal: the
# effects work without them.
function Register-App {
  try {
    New-Item -Force $UninstallKey | Out-Null
    $sizeKb = [int]((Get-ChildItem $ProgramDir -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
    $strings = [ordered]@{
      DisplayName          = 'GamersEqualizer'
      Publisher            = 'Jiri Nemec'
      DisplayIcon          = "$InstalledExe,0"
      InstallLocation      = $ProgramDir
      UninstallString      = "`"$InstalledExe`" --uninstall"
      QuietUninstallString = "`"$InstalledExe`" --uninstall --silent"
      URLInfoAbout         = 'https://github.com/PixCZ/GamersEqualizer'
    }
    if ($AppVersion) { $strings.DisplayVersion = $AppVersion }
    foreach ($k in $strings.Keys) { New-ItemProperty $UninstallKey -Name $k -Value $strings[$k] -PropertyType String -Force | Out-Null }
    foreach ($k in 'NoModify', 'NoRepair') { New-ItemProperty $UninstallKey -Name $k -Value 1 -PropertyType DWord -Force | Out-Null }
    New-ItemProperty $UninstallKey -Name EstimatedSize -Value $sizeKb -PropertyType DWord -Force | Out-Null
    $link = (New-Object -ComObject WScript.Shell).CreateShortcut($StartMenuLink)
    $link.TargetPath = $InstalledExe
    $link.WorkingDirectory = $ProgramDir
    $link.Save()
  }
  catch { Say "The Add or remove programs entry or the Start menu shortcut could not be made: $_" Yellow }
}

$AudioRoot = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$FxGuid = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$NullGuid = '{00000000-0000-0000-0000-000000000000}'
# Fixed Microsoft Windows SDK PKEY_FX composite (Windows 10 1803+, REG_MULTI_SZ) and legacy (REG_SZ) slots.
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

# An endpoint whose driver declares no effects (inbox "High Definition Audio Device", VMs) has no
# FxProperties, and Administrators may not create subkeys under an endpoint (SetValue + ReadKey
# only). The backup/restore privileges, which Administrators hold but must enable, create and
# delete the key without changing the owner or ACL of anything that exists; the new key inherits
# the endpoint's ACL. C# 5 (Windows PowerShell's compiler). Must match apo-uninstall.ps1.
$RestoreKeySource = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace GamersEqualizer {
  public static class RestoreKey {
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct TokenPrivilege { public uint Count; public long Luid; public uint Attributes; }

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPrivilege state, uint length, IntPtr previous, IntPtr returnLength);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern int RegCreateKeyExW(UIntPtr root, string subKey, uint reserved, string cls, uint options, uint access, IntPtr security, out IntPtr key, out uint disposition);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern int RegEnumKeyExW(IntPtr key, uint index, StringBuilder name, ref uint nameLength, IntPtr reserved, IntPtr cls, IntPtr clsLength, IntPtr lastWrite);
    [DllImport("advapi32.dll")]
    static extern int RegCloseKey(IntPtr key);
    [DllImport("ntdll.dll")]
    static extern int NtDeleteKey(IntPtr key);

    static readonly UIntPtr LocalMachine = new UIntPtr(0x80000002u);

    static void Enable(string privilege) {
      IntPtr token;
      // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
      if (!OpenProcessToken(GetCurrentProcess(), 0x28, out token)) throw new Win32Exception();
      try {
        TokenPrivilege state = new TokenPrivilege();
        state.Count = 1;
        state.Attributes = 2; // SE_PRIVILEGE_ENABLED
        if (!LookupPrivilegeValueW(null, privilege, out state.Luid)) throw new Win32Exception();
        if (!AdjustTokenPrivileges(token, false, ref state, 0, IntPtr.Zero, IntPtr.Zero)) throw new Win32Exception();
        // ERROR_NOT_ALL_ASSIGNED: the token lacks the privilege.
        int error = Marshal.GetLastWin32Error();
        if (error != 0) throw new Win32Exception(error, privilege + ": " + new Win32Exception(error).Message);
      }
      finally { CloseHandle(token); }
    }

    // REG_OPTION_BACKUP_RESTORE: the access comes from the privileges, not from the key's ACL.
    static IntPtr Open(string subKey, out bool created) {
      Enable("SeBackupPrivilege");
      Enable("SeRestorePrivilege");
      IntPtr key;
      uint disposition;
      int rc = RegCreateKeyExW(LocalMachine, subKey, 0, null, 4, 0, IntPtr.Zero, out key, out disposition);
      if (rc != 0) throw new Win32Exception(rc);
      created = disposition == 1; // REG_CREATED_NEW_KEY
      return key;
    }

    // True when the key was new.
    public static bool Create(string subKey) {
      bool created;
      RegCloseKey(Open(subKey, out created));
      return created;
    }

    // The key and every subkey (Windows adds some under FxProperties).
    public static void Delete(string subKey) {
      bool created;
      IntPtr key = Open(subKey, out created);
      try {
        List<string> children = new List<string>();
        for (uint i = 0; ; i++) {
          StringBuilder name = new StringBuilder(256);
          uint length = (uint)name.Capacity;
          int rc = RegEnumKeyExW(key, i, name, ref length, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
          if (rc == 259) break; // ERROR_NO_MORE_ITEMS
          if (rc != 0) throw new Win32Exception(rc);
          children.Add(name.ToString());
        }
        foreach (string child in children) Delete(subKey + "\\" + child);
        int status = NtDeleteKey(key);
        if (status != 0) throw new Win32Exception("NtDeleteKey failed: 0x" + status.ToString("X8"));
      }
      finally { RegCloseKey(key); }
    }
  }
}
'@

# Add-Type compiles through files in $env:TEMP, which non-admin processes can change between the
# write and the compile: a folder only administrators can write to instead, beside the app folder
# so it never blocks an uninstall.
function Use-RestoreKey {
  if ('GamersEqualizer.RestoreKey' -as [type]) { return }
  $temp = "$ProgramDir.tmp.$([guid]::NewGuid())"
  $saved = $env:TEMP, $env:TMP
  New-Item -ItemType Directory $temp | Out-Null
  try {
    $env:TEMP = $temp
    $env:TMP = $temp
    Add-Type -TypeDefinition $RestoreKeySource
  }
  finally {
    $env:TEMP, $env:TMP = $saved
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
  }
}

# $subKey under HKLM. True when the key was new.
function New-FxKey([string]$subKey) {
  # The privileged create would also make a missing endpoint key (device removed meanwhile).
  $endpoint = $subKey.Substring(0, $subKey.LastIndexOf('\'))
  if (-not (Test-RegKey $endpoint)) { throw "Endpoint not found: HKLM\$endpoint" }
  Use-RestoreKey
  [GamersEqualizer.RestoreKey]::Create($subKey)
}

# Looked up in the parent's subkey names: a key the ACL hides from Administrators still counts.
function Test-RegKey([string]$subKey) {
  $cut = $subKey.LastIndexOf('\')
  $parent = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey.Substring(0, $cut), $false)
  if ($null -eq $parent) { return $false }
  try { @($parent.GetSubKeyNames()) -contains $subKey.Substring($cut + 1) }
  finally { $parent.Dispose() }
}

# Never on a missing key: the privileged open would create it, and every missing parent.
function Remove-FxKey([string]$subKey) {
  if (-not (Test-RegKey $subKey)) { return }
  Use-RestoreKey
  [GamersEqualizer.RestoreKey]::Delete($subKey)
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

# The snapshot in the JSON form Restore-FxFromJson in apo-uninstall.ps1 reads: kinds by name,
# binary values as base64, multi-strings always as arrays.
# Values are assigned, not piped: Windows PowerShell 5.1 writes a piped array as {"value":[..],"Count":n}.
function ConvertTo-FxJson($snapshot) {
  $values = @(foreach ($rec in @($snapshot.Records)) {
      $kind = [Microsoft.Win32.RegistryValueKind]$rec.Kind
      $value = $rec.Value
      if ($kind -eq [Microsoft.Win32.RegistryValueKind]::Binary) { $value = [Convert]::ToBase64String([byte[]]@($rec.Value)) }
      elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::MultiString) {
        [string[]]$multi = @($rec.Value | Where-Object { $null -ne $_ })
        $value = $multi
      }
      [pscustomobject]@{ Name = [string]$rec.Name; Kind = $kind.ToString(); Value = $value }
    })
  # Created: the endpoint had no FxProperties key; apo-uninstall.ps1 deletes the one made for ours.
  [pscustomobject]@{ Flow = $snapshot.Flow; Id = $snapshot.Id; Created = [bool]$snapshot.Created; Values = $values } |
    ConvertTo-Json -Depth 5
}

# The rollback target: the snapshot without our effects. On an update it holds them already, and a
# rollback must take them out (audiodg crashed, the device did not open). A list that held only
# ours goes, as before the first install made it; the driver's own empty list stays.
function Remove-OurEffects($snapshot) {
  $ours = @($Classes.Values | ForEach-Object { $_.Clsid })
  $records = @(foreach ($rec in @($snapshot.Records)) {
      if ([Microsoft.Win32.RegistryValueKind]$rec.Kind -ne [Microsoft.Win32.RegistryValueKind]::MultiString) { $rec; continue }
      $all = @($rec.Value | Where-Object { $null -ne $_ })
      [string[]]$rest = @($all | Where-Object { $ours -notcontains $_ })
      if ($rest.Count -gt 0 -or $all.Count -eq 0) { [pscustomobject]@{ Name = $rec.Name; Kind = $rec.Kind; Value = $rest } }
    })
  $copy = [pscustomobject]@{ Flow = $snapshot.Flow; Id = $snapshot.Id; Records = $records }
  if ($snapshot.Created) { $copy | Add-Member -NotePropertyName Created -NotePropertyValue $true }
  $copy
}

# Exact value-only restore via SetValue + QueryValues (reg.exe import fails on TrustedInstaller subkeys).
function Restore-FxSnapshot($snapshot) {
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($snapshot.Flow)\$($snapshot.Id)\FxProperties"
  # The install made this key (New-FxKey): back to no key at all.
  if ($snapshot.Created) {
    Remove-FxKey $subKey
    return
  }
  # Putting ours back after a rollback removed the key made for them.
  if (-not (Test-RegKey $subKey)) { $null = New-FxKey $subKey }
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

# True when the slot's composite list held one of the driver's effects (not ours) before this
# install: the pre-install snapshot, not the registry now (Migrate-LegacySlots has run by then).
function Test-SlotHadDriverEffects($snapshot, [string]$slot) {
  if (-not $snapshot) { return $false }
  $ours = @($Classes.Values | ForEach-Object { $_.Clsid })
  $rec = @($snapshot.Records) | Where-Object { $_.Name -eq $CompositeValues[$slot] } | Select-Object -First 1
  if (-not $rec) { return $false }
  # Parenthesized: Read-FxList returns its list as one object; piping the expression enumerates it.
  [bool]((Read-FxList $rec.Value) | Where-Object { $_ -ne $NullGuid -and $ours -notcontains $_ })
}

# True when a backup file says an earlier install made the FxProperties key; false when it does
# not or the file cannot be read.
function Test-BackupCreated([string]$backupFile) {
  [bool]$(try { (Get-Content $backupFile -Raw | ConvertFrom-Json).Created } catch { $false })
}

# Our effect runs in a slot only in the modes listed for it, so Default is added. Not where the
# driver's effects sit with modes of their own: Default would run them in a mode it never declared.
function Ensure-ProcessingMode($job, [string]$fxSubKey, [string]$slot) {
  $modeName = $ModeValues[$slot]
  if (-not $modeName) { return }
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $modes = Read-FxList $fx.$modeName
  if ($modes | Where-Object { $_ -ieq $DefaultProcessingMode }) { return }
  if ($modes.Count -gt 0 -and (Test-SlotHadDriverEffects $script:FxSnapshots["$($job.Flow):$($job.Id)"] $slot)) {
    Say "$($job.Name) $slot`: the driver's effects there run only in modes $($modes -join ', '); Default not added, so ours may not start in $slot."
    return
  }
  $modes.Add($DefaultProcessingMode)
  Set-FxValue $fxSubKey $modeName ([string[]]$modes.ToArray())
  Say "$($job.Name) $slot modes: $($modes -join ', ')"
}

# Why our effects cannot go onto an endpoint with these FxProperties values ($fx: Get-ItemProperty
# of the key), or nothing. A driver that has only the old pre-mix / post-mix effects (LFX ,1 /
# GFX ,2) loses them once any stream / mode / endpoint effect value exists. Migrate-LegacySlots
# carries the LFX over; nothing carries the GFX.
# shortcut: such an endpoint is refused, not migrated. Upgrade (GFX into the composite MFX list,
# as Windows maps it) once a tester has such a driver to try it on.
function Get-EndpointBlocker($fx) {
  if (-not $fx) { return }
  $gfx = [string]$fx."$FxGuid,2"
  if (-not $gfx.Trim() -or $gfx -eq $NullGuid) { return }
  $names = @($fx.PSObject.Properties.Name)
  if (5, 6, 7, 13, 14, 15 | Where-Object { $names -contains "$FxGuid,$_" }) { return }
  "its driver has only old-style effects, and one of them (post-mix effect $gfx) would stop working"
}

# The slots whose legacy values Migrate-LegacySlots moves: those our effects go into, plus SFX while
# the old pre-mix value (LFX ,1) is set, which stops running once our newer values exist.
function Get-MigrationSlots($fx, [string[]]$slots) {
  $lfx = [string]$fx."$FxGuid,1"
  $withLfx = @($slots) + @(if ($lfx.Trim() -and $lfx -ne $NullGuid) { 'SFX' })
  @($withLfx | Where-Object { $LegacyValues.ContainsKey($_) } | Select-Object -Unique)
}

# Windows ignores a composite list (,13 / ,14 / ,15) while that slot's legacy REG_SZ value
# (,1 or ,5 / ,6 / ,7) is set (e.g. Microsoft Audio Home Theater Effects on USB headsets/DACs);
# measured: legacy ,5/,6 shadow ,13/,14 and leave ,15 running. So only $slots (Get-MigrationSlots)
# are migrated: their legacy CLSIDs move into the composite lists (with streaming modes) and the
# legacy values go. The driver's legacy values on other slots stay as they are.
function Migrate-LegacySlots($job, [string[]]$slots) {
  $fx = Get-ItemProperty (Join-Path $job.Key 'FxProperties')
  $fxSubKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
  foreach ($s in (Get-MigrationSlots $fx $slots)) {
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
      $job.Migrated = $true
      Say "$($job.Name) promoted legacy $($presentNames -join '/') -> $compName`: $($compList -join ' -> ')"
    }
    foreach ($legacyName in $presentNames) {
      Set-FxValue $fxSubKey $legacyName @()
    }
  }
}

# Only the low bits are DEVICE_STATE_*; higher ones are other flags (0x10000001 is active too).
function Get-ActiveEndpoints([string]$flow) {
  Get-ChildItem (Join-Path $AudioRoot $flow) | Where-Object { ((Get-ItemProperty $_.PSPath).DeviceState -band 0xF) -eq 1 } | ForEach-Object {
    $props = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
    [pscustomobject]@{ Id = $_.PSChildName; Name = "$($props.$NameProp) ($($props.$DeviceDescProp))" }
  }
}

# Appends $class to the endpoint's composite list for $slot and ensures its streaming modes entry.
# Note: Migrate-LegacySlots has already moved the legacy REG_SZ value of $slot. Capture also leaves
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

# Writes an endpoint's effect values from a snapshot while Windows audio is stopped. Returns
# when audio started again.
function Set-JobFx($snapshot) {
  Stop-Audio
  try { Restore-FxSnapshot $snapshot }
  finally { $started = Start-Audio }
  $started
}

function Rollback-Job($job, [datetime]$sinceRestart) {
  $snap = $script:FxSnapshots["$($job.Flow):$($job.Id)"]
  if (-not $snap) {
    Say "  Rollback warning: snapshot not found for $($job.Name)." Yellow
    return $sinceRestart
  }
  $started = Set-JobFx $snap
  Say "  $(Get-RollbackLine $job)" Yellow
  $started
}

# The endpoint did not open with our effects ($res from Invoke-EndpointProbe). After an audiodg
# crash, or when the check itself did not run or hung (Ran = $false), they come out at once.
# Otherwise they come out and it is opened again: when it opens now, ours were the cause and stay
# out. When it fails without them too (another app holds it in exclusive mode, privacy settings
# block the microphone), ours go back in, unchecked; not where the driver's effects were moved
# for them (Migrated): nothing then shows that this device runs the moved effects. Returns when
# audio last started.
function Resolve-ProbeOpen($job, $res, [datetime]$sinceRestart) {
  $snap = $script:FxSnapshots["$($job.Flow):$($job.Id)"]
  $installed = $null
  if (-not $res.Crashed -and $res.Ran -ne $false -and -not $job.Migrated -and $snap) {
    try { $installed = Get-FxSnapshot $job.Flow $job.Id } catch { Say "  Reading the effect settings failed: $_" Yellow }
  }
  if (-not $installed) { return Rollback-Job $job $sinceRestart }
  Say "  Opening the $($job.Word) again without GamersEqualizer, to see whether it is the cause..."
  $started = Set-JobFx $snap
  $baseline = Invoke-EndpointProbe $job $started
  if ($baseline.OpenOk -or $baseline.Crashed -or $baseline.Ran -eq $false) {
    Say "  $(Get-RollbackLine $job)" Yellow
    return $started
  }
  # Still rolled back if this fails: the app must learn that before the trap ends the run.
  try { $started = Set-JobFx $installed }
  catch {
    Say "  $(Get-RollbackLine $job)" Yellow
    throw
  }
  Say "  The $($job.Word) does not open without GamersEqualizer either (another app may use it exclusively, or privacy settings block it): GamersEqualizer stays on it, unchecked. Try again when it is free." Yellow
  $started
}

# The app reads `[Flow:{guid}]` from this line (apo_install::rolled_back_endpoint) and stops
# offering that device.
function Get-RollbackLine($job) {
  "Rolled back $($job.Name) [$($job.Flow):$($job.Id)] to its pre-install effect settings."
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
      return [pscustomobject]@{ OpenOk = $false; Crashed = $false; Ran = $false; Notes = @(); PreNotes = $preNotes }
    }
  }
  catch {
    Say "  The check could not start: $_" Red
    return [pscustomobject]@{ OpenOk = $false; Crashed = $false; Ran = $false; Notes = @(); PreNotes = $preNotes }
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

# Puts an APO class into $slot (migrating that slot's legacy value first, removing it from the
# other of EFX/MFX), restarting Windows Audio once.
function Move-EndpointSlot($job, [string]$class, [string]$slot) {
  Stop-Audio
  try {
    Migrate-LegacySlots $job @($slot)
    Set-EffectSlot $job $class $slot -RemoveFromOther
  }
  finally { $started = Start-Audio }
  $started
}

# Capture keeps the slot it was moved to, so an update does not undo the move.
function Get-DefaultSlot([string]$flow, [string]$endpointKey) {
  if ($flow -ne 'Capture') { return 'EFX' }
  $mfx = (Get-ItemProperty (Join-Path $endpointKey 'FxProperties') -ErrorAction SilentlyContinue).($CompositeValues.MFX)
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
  $fxKey = Join-Path $endpointKey 'FxProperties'
  if (-not $slot) { $slot = Get-DefaultSlot $flow $endpointKey }
  if ($CompositeValues.Keys -notcontains $slot) { throw "No effect slot for $($ep.Name) ('$slot'); nothing changed." }
  [pscustomobject]@{
    Flow = $flow; Id = $endpointId; Name = $ep.Name; Word = $word; Key = $endpointKey
    Targets = Get-Targets $flow $slot $noStreamEq
    # Set by Migrate-LegacySlots: the driver's own effects were moved into composite lists.
    Migrated = $false
    # No effect list yet (inbox "High Definition Audio Device", VMs): the install makes one (New-FxKey).
    NoFx = -not (Test-Path $fxKey)
    # Why this endpoint must stay as it is, or nothing; the callers below never install such a job.
    Blocker = Get-EndpointBlocker (Get-ItemProperty $fxKey -ErrorAction SilentlyContinue)
  }
}

# --- App only (no audio device): exe, DLL, entry; no endpoint, no audio restart -----------
if ($AppOnly) {
  if (-not ($AppExe -and $AppExeSha256 -and (Test-Path $AppExe))) { throw '-AppOnly needs -AppExe and -AppExeSha256.' }
  New-Item -ItemType Directory -Force $InstallDir | Out-Null
  # Staged and hash-checked like the full install below: both come from a user-writable folder.
  foreach ($copy in @(@($SourceDll, $InstalledDll, $SourceDllSha256), @($AppExe, $InstalledExe, $AppExeSha256))) {
    $source, $target, $hash = $copy
    if ([IO.Path]::GetFullPath($source) -ieq $target) { continue }
    Copy-Item $source "$target.new" -Force
    if ($hash -and (Get-FileHash "$target.new" -Algorithm SHA256).Hash -ne $hash) {
      Remove-Item "$target.new" -Force
      throw "$source changed after the app checked it; nothing installed."
    }
    Get-Process gamersequalizer -ErrorAction SilentlyContinue | Where-Object { $_.Path -ieq $target } | Stop-Process -Force
    Move-Item "$target.new" $target -Force
    Unblock-File $target
  }
  Register-App
  Say "App: $InstalledExe (no audio device: the effects are added from the app later)"
  return
}

# --- Check every endpoint first: an error leaves the system untouched ----------------------
$jobs = @()
$skipped = @()
if ($Endpoint) {
  # `powershell -File ... -Endpoint a,b` (how the app starts this) passes ONE string "a,b";
  # only `-Command` or a call from PowerShell makes it an array. Split either way.
  foreach ($spec in ($Endpoint -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    $flowName, $id, $specSlot = $spec -split ':', 3
    if ($DeviceWords.Keys -notcontains $flowName -or -not $id) { throw "Bad -Endpoint '$spec' (expected Capture:{guid} or Render:{guid})." }
    if ($specSlot -and ($flowName -ne 'Capture' -or @('EFX', 'MFX') -notcontains $specSlot)) {
      throw "Bad -Endpoint '$spec' (a slot is only Capture:{guid}:EFX or Capture:{guid}:MFX)."
    }
    $job = Resolve-Job $flowName $id $specSlot $false
    # The app passes every active device at once: one that cannot take the effects is left
    # untouched and named (again at the end), the others are installed.
    if ($job.Blocker) { $skipped += "Skipped $($job.Name): $($job.Blocker). It was not changed." }
    else { $jobs += $job }
  }
  if (-not $jobs) { throw "$($skipped -join ' ') Nothing was changed." }
}
else {
  $job = Resolve-Job $Flow $EndpointId $Slot $NoStreamEq.IsPresent
  if ($job.Blocker) { throw "Cannot install on $($job.Name): $($job.Blocker). Nothing was changed." }
  $jobs += $job
}
foreach ($line in $skipped) { Say $line Yellow }
if (-not (Test-Path $SourceDll)) {
  throw "DLL not found: $SourceDll. Build it first: cargo build --profile apo -p gamersequalizer-apo"
}
Say 'Do not click inside this window: a click pauses it until you press Enter.'
foreach ($job in $jobs) {
  $what = ($job.Targets | ForEach-Object { "$($Classes[$_[0]].Name) ($($_[1]))" }) -join ', '
  Say "Target: $($job.Name) $($job.Id)`: $what"
}

# --- Snapshot: in memory for the rollback below (without ours), and on disk for apo-uninstall.ps1
# The disk copy is the driver's own effects without ours. It is refreshed while the endpoint holds
# none of our effects (a driver update or a rollback since the last install changed them), and
# kept while ours are in it: then it is the last state from before GamersEqualizer.
$FxSnapshots = @{}
New-Item -ItemType Directory -Force $BackupDir | Out-Null
foreach ($job in @($jobs)) {
  $made = $false
  if ($job.NoFx) {
    # The first change to such an endpoint: when it fails, the endpoint is left as it was and skipped.
    # Only a key this run made counts as made: one that appeared since Resolve-Job is the driver's.
    $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($job.Flow)\$($job.Id)\FxProperties"
    try {
      $made = New-FxKey $subKey
      $script:MadeKeysPending = $script:MadeKeysPending -or $made
      $snap = Get-FxSnapshot $job.Flow $job.Id
    }
    catch {
      $reason = $_
      if ($made) {
        try { Remove-FxKey $subKey } catch { Say "Removing the effect list made for $($job.Name) failed: $_" Red }
      }
      $skipped += "Skipped $($job.Name): it had no Windows effect list and making one failed ($reason). It was not changed."
      $jobs = @($jobs | Where-Object { $_ -ne $job })
      continue
    }
    if ($made) {
      $sddl = try { (Get-Acl "HKLM:\$subKey").Sddl } catch { "unreadable: $_" }
      Say "$($job.Name) had no Windows effect list: made one (rollback and uninstall delete it). ACL: $sddl"
    }
  }
  else { $snap = Get-FxSnapshot $job.Flow $job.Id }
  $backupFile = Join-Path $BackupDir "$($job.Id)-FxProperties-original.json"
  # A key an earlier install made, left empty by a failed delete, is still to be deleted.
  if (-not $made -and @($snap.Records).Count -eq 0 -and (Test-Path $backupFile)) {
    $made = Test-BackupCreated $backupFile
  }
  if ($made) { $snap | Add-Member -NotePropertyName Created -NotePropertyValue $true }
  $FxSnapshots["$($job.Flow):$($job.Id)"] = Remove-OurEffects $snap
  $json = ConvertTo-FxJson $snap
  $holdsOurs = @($Classes.Values | Where-Object { $json -match [regex]::Escape($_.Clsid) }).Count -gt 0
  if (-not $holdsOurs -or -not (Test-Path $backupFile)) {
    [IO.File]::WriteAllText($backupFile, $json, (New-Object Text.UTF8Encoding $false))
    Say "Backup of $($job.Name)'s effect settings: $backupFile"
  }
}
if (-not $jobs) { throw "$($skipped -join ' ') Nothing was changed." }

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
# From here the catch below and the self-test's rollbacks undo made keys (Restore-FxSnapshot).
$MadeKeysPending = $false
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
    Migrate-LegacySlots $job @($job.Targets | ForEach-Object { $_[1] })
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
      try { Restore-FxSnapshot $snap }
      catch { Say "Restoring $($snap.Flow) $($snap.Id) failed: $_" Red }
    }
  }
  throw
}
finally {
  $audioStarted = Start-Audio
}
if (-not $NoAppInstall -and (Test-Path $InstalledExe)) { Register-App }

foreach ($job in $jobs) {
  if (Test-EnhancementsOff $job) {
    Write-Warning "Audio enhancements are OFF for $($job.Name); turn them on (Settings > Sound > device > Audio enhancements), or the APO is not loaded."
  }
}

# --- Self-test & safe automatic rollback on open failure or audiodg crash -----------------
# A skipped device also keeps the window open at the end.
$selfTestFailed = [bool]$skipped
$ProbeTemp = Join-Path $InstallDir 'gamersequalizer-probe.exe'
$ProbeCopy = Get-ProbeExe

# Immediate startup crash check (also without the probe exe): if audiodg crashed as audio
# started, undo everything at once.
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
  foreach ($job in $jobs) { Say "  $(Get-RollbackLine $job)" Yellow }
  $selfTestFailed = $true
  $jobs = @()
}

# Did Windows start our effect on this endpoint? A note tagged with its GUID says so. An untagged
# note (Windows gave the effect no endpoint) counts only when this is the only endpoint of its
# flow being installed: with several, it may be another device's.
function Test-ApoNoteStarted($res, [string]$className, [string]$guid, [string]$tailPattern, [bool]$onlyOfFlow) {
  $clsPat = [regex]::Escape($className)
  $idPat = [regex]::Escape($guid)
  $all = @($res.Notes + $res.PreNotes)
  if (@($all | Where-Object { $_ -match "$clsPat \[$idPat\]: $tailPattern" }).Count -gt 0) { return $true }
  if (-not $onlyOfFlow) { return $false }
  if (@($res.Notes | Where-Object { $_ -match "$clsPat`: $tailPattern" }).Count -gt 0) { return $true }
  if (@($all | Where-Object { $_ -match "$clsPat \[\{[0-9a-f-]+\}\]:" }).Count -eq 0 -and
      @($res.PreNotes | Where-Object { $_ -match "$clsPat`: $tailPattern" }).Count -gt 0) {
    return $true
  }
  $false
}

foreach ($job in ($jobs | Where-Object { $_.Flow -eq 'Capture' })) {
  if (-not $ProbeCopy -or (Test-EnhancementsOff $job)) { continue }
  $onlyOfFlow = @($jobs | Where-Object { $_.Flow -eq 'Capture' }).Count -eq 1
  $slot = $job.Targets[0][1]
  Say "`nChecking $($job.Name): opening microphone for a few seconds..."
  $res = Invoke-EndpointProbe $job $audioStarted
  if (-not $res.OpenOk) {
    $audioStarted = Resolve-ProbeOpen $job $res $audioStarted
    $selfTestFailed = $true
    continue
  }
  if (Test-ApoNoteStarted $res 'GamersEqualizer Mic Chain' $job.Id '(stream |refused a stream)' $onlyOfFlow) {
    Say "  Works: Windows starts the mic effect ($slot)." Green
    continue
  }
  $other = if ($slot -eq 'MFX') { 'EFX' } else { 'MFX' }
  Say "  Windows did not start it in $slot; moving it to $other (audio restarts once more)..."
  $audioStarted = Move-EndpointSlot $job 'Capture' $other
  $res2 = Invoke-EndpointProbe $job $audioStarted
  if (-not $res2.OpenOk) {
    $audioStarted = Resolve-ProbeOpen $job $res2 $audioStarted
    $selfTestFailed = $true
    continue
  }
  if (Test-ApoNoteStarted $res2 'GamersEqualizer Mic Chain' $job.Id '(stream |refused a stream)' $onlyOfFlow) {
    Say "  Works: Windows starts the mic effect ($other)." Green
    continue
  }
  # The driver's effects were moved into composite lists and ours runs in neither slot: nothing
  # shows that Windows reads those lists on this microphone, so its effects go back as they were.
  if ($job.Migrated) {
    Say '  Windows starts the mic effect neither in EFX nor in MFX: undoing the changes to this microphone.' Red
    $audioStarted = Rollback-Job $job $audioStarted
    $selfTestFailed = $true
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
  $onlyOfFlow = @($jobs | Where-Object { $_.Flow -eq 'Render' }).Count -eq 1
  $hasOut = @($job.Targets | Where-Object { $_[0] -eq 'Render' }).Count -gt 0
  $hasStream = @($job.Targets | Where-Object { $_[0] -eq 'Stream' }).Count -gt 0
  Say "`nChecking $($job.Name): opening silent output for a moment..."
  $res = Invoke-EndpointProbe $job $audioStarted
  if (-not $res.OpenOk) {
    $audioStarted = Resolve-ProbeOpen $job $res $audioStarted
    $selfTestFailed = $true
    continue
  }
  $outOk = -not $hasOut -or (Test-ApoNoteStarted $res 'GamersEqualizer Output EQ' $job.Id 'stream .* started' $onlyOfFlow)
  $streamOk = -not $hasStream -or (Test-ApoNoteStarted $res 'GamersEqualizer Stream EQ' $job.Id 'stream .* started' $onlyOfFlow)
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
      $audioStarted = Resolve-ProbeOpen $job $res2 $audioStarted
      $selfTestFailed = $true
      continue
    }
    $outOk2 = Test-ApoNoteStarted $res2 'GamersEqualizer Output EQ' $job.Id 'stream .* started' $onlyOfFlow
    $streamOk2 = -not $hasStream -or (Test-ApoNoteStarted $res2 'GamersEqualizer Stream EQ' $job.Id 'stream .* started' $onlyOfFlow)
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
foreach ($line in $skipped) { Say $line Yellow }
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
