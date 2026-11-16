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

# shortcut: Say, Set-FxValue, the restore and the CLSIDs below are variants of apo-install.ps1's;
# what both have the same is in apo-common.ps1. Merge them there once the change can be tried
# on Windows (they differ in behavior, so it is more than a move).

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
. "$PSScriptRoot\apo-common.ps1"  # @common
# Older versions backed up here. Any user can create this folder (or a junction to a system
# folder), so it is never deleted recursively: only our .reg files, and not through a link.
$LegacyBackupRoot = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'GamersEqualizer'
$MmDevices = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
# Composite list, then the legacy values (as in apo-install.ps1). GFX (,2) is not a slot here.
$SlotEffectValues = @{
  SFX = @("$FxGuid,13", "$FxGuid,5", "$FxGuid,1")
  MFX = @("$FxGuid,14", "$FxGuid,6")
  EFX = @("$FxGuid,15", "$FxGuid,7")
}
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

  $keys = @('HKLM:\SOFTWARE\GamersEqualizer', $UninstallKey)
  if (Test-Path $StartMenuLink) { Remove-Item $StartMenuLink -Force }
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


# The installed app started this: the window exits right after, `gamersequalizer.exe
# --uninstall` (Add or remove programs, winget) waits for this script. A running exe cannot be
# deleted but can be renamed, so what is still held moves out of the app folder.
# shortcut: a moved file stays in the temp folder until Windows cleans it up; delete it on
# reboot (MoveFileEx) if leftovers there ever matter.
foreach ($file in $InstalledExe, "$InstalledExe.new") {
  if (-not (Test-Path $file)) { continue }
  Remove-Item $file -Force -ErrorAction SilentlyContinue
  if (Test-Path $file) {
    try { Move-Item $file (Join-Path ([IO.Path]::GetTempPath()) "gamersequalizer-removed-$([guid]::NewGuid()).exe") -Force }
    catch { Say "Could not delete $file (still running?); delete it by hand." Yellow }
  }
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
