#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Removes GamersEqualizer APOs everywhere they were added.

.DESCRIPTION
  First puts back the effect settings each endpoint had before GamersEqualizer first touched it
  (the JSON backups apo-install.ps1 keeps in %ProgramFiles%\GamersEqualizer\apo-backup: the
  effect lists and their processing modes only, so a legacy value the installer folded into a
  composite list comes back; an FxProperties key the installer made, backup marked Created, is
  deleted). Then removes our CLSIDs from every capture/render endpoint's effect
  lists (other effects stay, String vs MultiString kinds are preserved), cleans up Default-only
  streaming modes on endpoints without a backup where the slot has no effects left, unregisters
  the COM classes and APOs, deletes the config key, the installed DLL, the installed app (once it
  has exited) and any temporary scripts in %LOCALAPPDATA%\GamersEqualizer\scripts, and restarts
  the Windows Audio service. Installs older than the backups simply have none to restore.
  The backup folder is deleted only when every backup in it was restored.
  -RestoreBackup <file.json | file.reg | dir> restores other manual/legacy backups in full:
  JSON via .NET RegistryKey (SetValue + QueryValues); legacy .reg files via reg.exe import.
  -RemoveBackups also deletes the .reg files older versions left in %ProgramData%\GamersEqualizer;
  the app's "Uninstall from Windows" button passes it.
  Writes nothing to its window while Windows audio is stopped (a click into the window would
  pause it there); every line also goes to the Application log for the app's log.
#>
param(
  [string]$RestoreBackup,
  [switch]$RemoveBackups,
  # Started by the app: keep the window open on errors, briefly on success.
  [switch]$FromApp,
  # Passed by the app so UAC under another admin account still cleans the caller's staged scripts.
  [string]$ScriptDir
)

$ErrorActionPreference = 'Stop'
trap {
  # A trap runs before `finally`; if the user closes this window at the prompt below, the
  # `finally` that restarts audio never runs. Start it here first (no-op when it is running).
  Start-Service Audiosrv -ErrorAction SilentlyContinue -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  if ($FromApp) {
    Say "`nUninstall failed: $_" Red
    Read-Host 'Press Enter to close'
    exit 1
  }
  break
}

# shortcut: the registry helpers and constants below are copies of apo-install.ps1's (Set-FxValue,
# the restore, RestoreKey / Remove-FxKey, CLSIDs, value names, Say). Merge both into one script with an -Uninstall mode once
# the merged script can be tried on Windows; a dot-sourced module would be a third file outside
# the hash check in elevated_bootstrap.

# As in apo-install.ps1: nothing is written to the window while Windows audio is stopped (a
# click into it pauses the script at its next output); the lines are held until audio runs.
$AudioStopped = $false
$Held = New-Object System.Collections.Generic.List[string]

# One line for the window and for the app's log (Application log, source GamersEqualizer; the
# source is deleted at the end, its entries stay).
function Say([string]$text, [string]$color) {
  if ($text.Trim()) {
    try { [System.Diagnostics.EventLog]::WriteEntry('GamersEqualizer', "uninstaller: $($text.Trim())") } catch { }
  }
  if ($script:AudioStopped) { $Held.Add($text) }
  elseif ($color) { Write-Host $text -ForegroundColor $color }
  else { Write-Host $text }
}

# Mic chain, output EQ and stream EQ (all in the same DLL).
$Clsids = @('{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}', '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}', '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}')
# Known folders, not $env:ProgramFiles / $env:ProgramData (user-overridable, inherited here).
$ProgramDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer'
$InstallDir = Join-Path $ProgramDir 'apo'
$InstalledExe = Join-Path $ProgramDir 'gamersequalizer.exe'
$BackupDir = Join-Path $ProgramDir 'apo-backup'
# Older versions backed up here. Any user can create this folder (or a junction to a system
# folder), so it is never deleted recursively: only our .reg files, and not through a link.
$LegacyBackupRoot = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'GamersEqualizer'
$MmDevices = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$FxGuid = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$NullGuid = '{00000000-0000-0000-0000-000000000000}'
# Composite list, then the legacy values (as in apo-install.ps1). GFX (,2) is not a slot here.
$SlotEffectValues = @{
  SFX = @("$FxGuid,13", "$FxGuid,5", "$FxGuid,1")
  MFX = @("$FxGuid,14", "$FxGuid,6")
  EFX = @("$FxGuid,15", "$FxGuid,7")
}
$ModesGuid = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$DefaultProcessingMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'
$ModeValues = @{ SFX = "$ModesGuid,5"; MFX = "$ModesGuid,6"; EFX = "$ModesGuid,7" }
# Every value apo-install.ps1 writes: what its backups put back. Driver properties next to them
# stay as they are now, also when a driver update changed them after the install.
$InstallerValues = @($SlotEffectValues.Values | ForEach-Object { $_ }) + @($ModeValues.Values)

function Read-FxGuids([Microsoft.Win32.RegistryKey]$regKey, [string]$valueName) {
  $raw = $regKey.GetValue($valueName, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
  if ($null -eq $raw) { return @() }
  $out = New-Object System.Collections.Generic.List[string]
  foreach ($item in @($raw)) {
    $s = ([string]$item).Trim()
    if ($s -and $s -ne $NullGuid) { $out.Add($s) }
  }
  $out.ToArray()
}

# FxProperties grants Administrators only SetValue + QueryValues (ReadKey); the PowerShell
# registry provider asks for more and gets "access denied", so open via .NET RegistryKey.
function Set-FxValue([string]$subKey, [string]$name, [string[]]$value, [Microsoft.Win32.RegistryValueKind]$kind = [Microsoft.Win32.RegistryValueKind]::MultiString) {
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    [string[]]$clean = @($value | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) })
    if ($clean.Count -eq 0) {
      $key.DeleteValue($name, $false)
    }
    elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::String) {
      $key.SetValue($name, [string]$clean[0], [Microsoft.Win32.RegistryValueKind]::String)
    }
    else {
      $key.SetValue($name, $clean, [Microsoft.Win32.RegistryValueKind]::MultiString)
    }
  }
  finally { $key.Dispose() }
}

# An endpoint whose driver declares no effects (inbox "High Definition Audio Device", VMs) has no
# FxProperties, and Administrators may not create subkeys under an endpoint (SetValue + ReadKey
# only). The backup/restore privileges, which Administrators hold but must enable, create and
# delete the key without changing the owner or ACL of anything that exists; the new key inherits
# the endpoint's ACL. C# 5 (Windows PowerShell's compiler). Copy of apo-install.ps1's.
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

function Use-RestoreKey {
  if (-not ('GamersEqualizer.RestoreKey' -as [type])) { Add-Type -TypeDefinition $RestoreKeySource }
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

# Only delete our own staged scripts, never through a junction/reparse point.
function Remove-StagedScripts([string]$dirPath) {
  if ([string]::IsNullOrWhiteSpace($dirPath)) { return $false }
  $normPath = try { [IO.Path]::GetFullPath($dirPath).TrimEnd('\', '/') } catch { return $false }
  $scriptsDir = Get-Item -LiteralPath $normPath -Force -ErrorAction SilentlyContinue
  if (-not ($scriptsDir -is [IO.DirectoryInfo]) -or ($scriptsDir.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $false }
  if ($scriptsDir.Name -ine 'scripts') { return $false }
  $parentDir = $scriptsDir.Parent
  if (-not $parentDir -or $parentDir.Name -ine 'GamersEqualizer' -or ($parentDir.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $false }
  $removed = $false
  foreach ($fileName in 'apo-install.ps1', 'apo-uninstall.ps1') {
    $fileItem = Get-Item -LiteralPath (Join-Path $scriptsDir.FullName $fileName) -Force -ErrorAction SilentlyContinue
    if (($fileItem -is [IO.FileInfo]) -and -not ($fileItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      Remove-Item -LiteralPath $fileItem.FullName -Force -ErrorAction SilentlyContinue
      $removed = $true
    }
  }
  if (-not (Get-ChildItem -LiteralPath $scriptsDir.FullName -Force -ErrorAction SilentlyContinue)) {
    Remove-Item -LiteralPath $scriptsDir.FullName -Force -ErrorAction SilentlyContinue
    $removed = $true
  }
  if (-not (Get-ChildItem -LiteralPath $parentDir.FullName -Force -ErrorAction SilentlyContinue)) {
    Remove-Item -LiteralPath $parentDir.FullName -Force -ErrorAction SilentlyContinue
    $removed = $true
  }
  return $removed
}

# $onlyNames: restore (and delete) only these values; all of them when empty.
# True when the text (a backup's JSON, or Get-FxText) names one of our effect CLSIDs.
function Test-HoldsOurEffects([string]$text) {
  @($Clsids | Where-Object { $text -match [regex]::Escape($_) }).Count -gt 0
}

# Every value of an endpoint's FxProperties as text ('' when the endpoint is gone).
function Get-FxText([string]$flow, [string]$id) {
  $path = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $props = Get-ItemProperty $path -ErrorAction SilentlyContinue
  if (-not $props) { return '' }
  ($props.PSObject.Properties | Where-Object { $_.Name -notlike 'PS*' } | ForEach-Object { @($_.Value) -join ';' }) -join "`n"
}

function Restore-FxFromJson([string]$jsonFile, [string[]]$onlyNames) {
  $parsed = Get-Content $jsonFile -Raw -Encoding UTF8 | ConvertFrom-Json
  $flow = [string]$parsed.Flow
  $id = [string]$parsed.Id
  if (-not $flow -or -not $id) {
    throw "Invalid JSON backup (missing Flow or Id): $jsonFile"
  }
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  if (-not (Test-Path "HKLM:\$subKey")) {
    Say "Skipping backup for missing endpoint $flow $id ($($jsonFile | Split-Path -Leaf))" Yellow
    return $id
  }
  $inScope = { param($n) -not $onlyNames -or $onlyNames -contains $n }
  $origRecords = @($parsed.Values | ForEach-Object { $_ } | Where-Object { & $inScope ([string]$_.Name) })
  $origNames = @($origRecords | ForEach-Object { [string]$_.Name })
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    foreach ($cur in @($key.GetValueNames())) {
      if ((& $inScope $cur) -and $origNames -notcontains $cur) { $key.DeleteValue($cur, $false) }
    }
    foreach ($rec in $origRecords) {
      $kind = [Microsoft.Win32.RegistryValueKind][Enum]::Parse([Microsoft.Win32.RegistryValueKind], [string]$rec.Kind)
      $valName = [string]$rec.Name
      if ($kind -eq [Microsoft.Win32.RegistryValueKind]::Binary) {
        [byte[]]$binVal = [Convert]::FromBase64String([string]$rec.Value)
        $key.SetValue($valName, $binVal, $kind)
      }
      elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::MultiString) {
        [string[]]$multiVal = @($rec.Value | ForEach-Object { [string]$_ })
        $key.SetValue($valName, $multiVal, $kind)
      }
      elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::DWord) {
        [int]$dwVal = [int]$rec.Value
        $key.SetValue($valName, $dwVal, $kind)
      }
      elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::QWord) {
        [long]$qwVal = [long]$rec.Value
        $key.SetValue($valName, $qwVal, $kind)
      }
      else {
        [string]$strVal = [string]$rec.Value
        $key.SetValue($valName, $strVal, $kind)
      }
    }
  }
  finally { $key.Dispose() }
  Say "Restored $flow $id from $($jsonFile | Split-Path -Leaf)"
  $id
}

# reg.exe prints to the window; its output goes nowhere, the exit code tells.
function Import-RegFile([string]$file) {
  cmd.exe /c "reg.exe import `"$file`" >nul 2>&1"
  if ($LASTEXITCODE -ne 0) {
    Say "reg import failed ($($file | Split-Path -Leaf), exit $LASTEXITCODE): FxProperties subkey ACLs often block reg.exe import" Yellow
  }
  else { Say "Restored $($file | Split-Path -Leaf)" }
}

if ($RestoreBackup -and -not (Test-Path $RestoreBackup)) { throw "Backup not found: $RestoreBackup" }

$script:AudioStopped = $true
Stop-Service Audiosrv -Force -WarningAction SilentlyContinue
# Endpoints whose effect settings came back from a backup: their modes are the driver's own.
$restoredIds = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
$backupsRestored = $true
try {
  # First the installer's own backups (before our CLSIDs are removed below: a backup made by an
  # update over an older install still lists them).
  foreach ($jf in @(Get-ChildItem -Path $BackupDir -Filter '*-FxProperties-original.json' -File -ErrorAction SilentlyContinue)) {
    try {
      $backup = Get-Content $jf.FullName -Raw -Encoding UTF8
      $parsed = $backup | ConvertFrom-Json
      $fxText = Get-FxText ([string]$parsed.Flow) ([string]$parsed.Id)
      if ($parsed.Created) {
        # The endpoint had no FxProperties. The key made for ours goes, unless other effects (another
        # equalizer, a driver update) use it now: then only ours are removed, by the loop below.
        $foreign = @([regex]::Matches($fxText, '\{[0-9a-fA-F-]{36}\}') | ForEach-Object { $_.Value } |
          Where-Object { $Clsids -notcontains $_ -and $_ -ne $NullGuid -and $_ -ne $DefaultProcessingMode })
        if ($foreign) {
          Say "Kept the effect list the installer made for $($parsed.Flow) $($parsed.Id): other effects use it now"
          continue
        }
        if (@('Capture', 'Render') -notcontains $parsed.Flow -or [string]$parsed.Id -notmatch '^\{[0-9a-fA-F-]+\}$') {
          throw "Invalid JSON backup (Flow or Id): $($jf.Name)"
        }
        Remove-FxKey "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($parsed.Flow)\$($parsed.Id)\FxProperties"
        Say "Removed the effect list the installer made for $($parsed.Flow) $($parsed.Id)"
        continue
      }
      if (-not (Test-HoldsOurEffects $fxText)) {
        # None of ours there any more (a driver update or a rollback since): the endpoint's
        # current settings are newer than the backup and are kept.
        Say "Skipping $($jf.Name): the endpoint no longer has our effects"
        continue
      }
      $rid = Restore-FxFromJson $jf.FullName $InstallerValues
      # A backup made by an update over an older install still holds our effects and the Default
      # mode we added: such an endpoint still gets the cleanup below.
      if ($rid -and -not (Test-HoldsOurEffects $backup)) { $null = $restoredIds.Add($rid) }
    }
    catch {
      $backupsRestored = $false
      Say "Restoring $($jf.Name) failed: $_" Red
    }
  }

  if ($RestoreBackup) {
    if ((Get-Item $RestoreBackup -ErrorAction SilentlyContinue) -is [System.IO.DirectoryInfo]) {
      $jsonFiles = @(Get-ChildItem -Path $RestoreBackup -Filter '*-FxProperties-original.json' -ErrorAction SilentlyContinue)
      if ($jsonFiles.Count -eq 0) { $jsonFiles = @(Get-ChildItem -Path $RestoreBackup -Filter '*-FxProperties-previous.json' -ErrorAction SilentlyContinue) }
      foreach ($jf in $jsonFiles) {
        $rid = Restore-FxFromJson $jf.FullName
        if ($rid) { $null = $restoredIds.Add($rid) }
      }
      # Fallback only for older installs that wrote .reg before JSON backups existed.
      $regFiles = @(Get-ChildItem -Path $RestoreBackup -Filter '*-FxProperties-original.reg' -ErrorAction SilentlyContinue)
      if ($regFiles.Count -eq 0) { $regFiles = @(Get-ChildItem -Path $RestoreBackup -Filter '*.reg' -ErrorAction SilentlyContinue) }
      foreach ($rf in $regFiles) {
        if ($rf.Name -match '^(\{[a-f0-9-]+\})-FxProperties-' -and $restoredIds.Contains($Matches[1])) { continue }
        Import-RegFile $rf.FullName
      }
    }
    elseif ($RestoreBackup -like '*.json') {
      $rid = Restore-FxFromJson $RestoreBackup
      if ($rid) { $null = $restoredIds.Add($rid) }
    }
    else {
      Import-RegFile $RestoreBackup
    }
  }

  foreach ($flow in 'Capture', 'Render') {
    foreach ($ep in Get-ChildItem (Join-Path $MmDevices $flow)) {
      $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$($ep.PSChildName)\FxProperties"
      $regKey = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, $false)
      if ($null -eq $regKey) { continue }
      try {
        $removedAny = $false
        foreach ($name in @($regKey.GetValueNames())) {
          if ($name -notlike '{*') { continue }
          $kind = $regKey.GetValueKind($name)
          if ($kind -ne [Microsoft.Win32.RegistryValueKind]::String -and $kind -ne [Microsoft.Win32.RegistryValueKind]::MultiString) { continue }
          $raw = $regKey.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
          $vals = @($raw | Where-Object { $null -ne $_ } | ForEach-Object { ([string]$_).Trim() } | Where-Object { $_ })
          $rest = @($vals | Where-Object { $Clsids -notcontains $_ })
          if ($rest.Count -eq $vals.Count) { continue }
          Set-FxValue $subKey $name ([string[]]$rest) $kind
          $removedAny = $true
          Say "Removed from $flow $($ep.PSChildName) $name"
        }
        # A manual .reg restore leaves no id behind: no cleanup then, as before.
        if ($removedAny -and -not $RestoreBackup -and -not $restoredIds.Contains($ep.PSChildName)) {
          $slots = if ($flow -eq 'Capture') { @('MFX', 'EFX') } else { @('SFX', 'MFX', 'EFX') }
          foreach ($slot in $slots) {
            $hasEffects = $false
            foreach ($fxName in $SlotEffectValues[$slot]) {
              if (@(Read-FxGuids $regKey $fxName).Count -gt 0) { $hasEffects = $true; break }
            }
            if ($hasEffects) { continue }
            $modeName = $ModeValues[$slot]
            $modes = @(Read-FxGuids $regKey $modeName)
            if ($modes.Count -eq 1 -and $modes[0] -ieq $DefaultProcessingMode) {
              Set-FxValue $subKey $modeName @()
              Say "Removed empty $slot mode $modeName from $flow $($ep.PSChildName)"
            }
          }
        }
      }
      finally { $regKey.Dispose() }
    }
  }

  $keys = @('HKLM:\SOFTWARE\GamersEqualizer')
  foreach ($c in $Clsids) {
    $keys += "HKLM:\SOFTWARE\Classes\CLSID\$c", "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$c"
  }
  foreach ($key in $keys) {
    if (Test-Path $key) { Remove-Item $key -Recurse -Force }
  }
  if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force }
  # Kept when a restore failed: the driver's settings are still in it (also for -RestoreBackup).
  if ((Test-Path $BackupDir) -and $backupsRestored) {
    Remove-Item $BackupDir -Recurse -Force
    Say "Removed backups in $BackupDir"
  }
  elseif (Test-Path $BackupDir) {
    Say "Kept $BackupDir (a restore failed): uninstall again to retry; the backups are used again." Yellow
  }
  if ($RemoveBackups) {
    $legacy = Get-Item $LegacyBackupRoot -Force -ErrorAction SilentlyContinue
    if ($legacy -and -not ($legacy.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      $legacyDir = Get-Item (Join-Path $LegacyBackupRoot 'apo-backup') -Force -ErrorAction SilentlyContinue
      if ($legacyDir -and -not ($legacyDir.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Get-ChildItem $legacyDir.FullName -File -Force -Filter '*-FxProperties-*' | Remove-Item -Force
        if (-not (Get-ChildItem $legacyDir.FullName -Force)) { Remove-Item $legacyDir.FullName -Force }
      }
      if (-not (Get-ChildItem $LegacyBackupRoot -Force)) { Remove-Item $LegacyBackupRoot -Force }
      Say "Removed old backups in $LegacyBackupRoot"
    }
  }
}
finally {
  Start-Service Audiosrv -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  foreach ($line in $Held) { Write-Host $line }
  $Held.Clear()
}


# The installed app started this and exits right after; its exe cannot go while it runs.
foreach ($file in $InstalledExe, "$InstalledExe.new") {
  for ($try = 0; $try -lt 20 -and (Test-Path $file); $try++) {
    Remove-Item $file -Force -ErrorAction SilentlyContinue
    if (Test-Path $file) { Start-Sleep -Seconds 1 }
  }
  if (Test-Path $file) { Say "Could not delete $file (still running?); delete it by hand." Yellow }
}
if ((Test-Path $ProgramDir) -and -not (Get-ChildItem $ProgramDir -Force)) { Remove-Item $ProgramDir -Force }

$scriptDirs = New-Object 'System.Collections.Generic.List[string]'
$localAppData = [Environment]::GetFolderPath('LocalApplicationData')
if ($localAppData) { $scriptDirs.Add((Join-Path $localAppData 'GamersEqualizer\scripts')) }
if ($ScriptDir) { $scriptDirs.Add($ScriptDir) }
$seenScriptDirs = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
foreach ($dir in $scriptDirs) {
  $normDir = try { [IO.Path]::GetFullPath($dir).TrimEnd('\', '/') } catch { $dir }
  if ($seenScriptDirs.Add($normDir) -and (Remove-StagedScripts $normDir)) {
    Say "Removed temporary scripts in $normDir"
  }
}

Say 'Uninstalled.'
# The notes for the app log registered this event source; last, since every Say writes to it.
try { [System.Diagnostics.EventLog]::DeleteEventSource('GamersEqualizer') } catch { }
if ($FromApp) { Start-Sleep -Seconds 3 }
