#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Removes GamersEqualizer APOs everywhere they were added.

.DESCRIPTION
  Removes our CLSIDs from every capture/render endpoint's effect lists (other effects stay
  and original String vs MultiString registry value kinds are preserved), cleans up any
  Default-only streaming mode values on touched endpoints where the slot now has no effects
  left, unregisters the COM classes and APOs, deletes the config key, the installed DLL,
  the installed app (once it has exited) and any temporary scripts in
  %LOCALAPPDATA%\GamersEqualizer\scripts, and restarts the Windows Audio service.
  Does not require any backup folder to run cleanly.
  -RestoreBackup <file.json | file.reg | dir> optionally restores exact pre-install
  FxProperties values if manual/legacy backups exist: JSON backups are restored directly via
  .NET RegistryKey (SetValue + QueryValues); legacy .reg files attempt reg.exe import.
  -RemoveBackups also deletes any legacy backups (%ProgramFiles%\GamersEqualizer\apo-backup,
  and the .reg files older versions left in %ProgramData%\GamersEqualizer); the app's
  "Uninstall from Windows" button passes it.
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
  Start-Service Audiosrv -ErrorAction SilentlyContinue
  if ($FromApp) {
    Write-Host "`nUninstall failed: $_" -ForegroundColor Red
    Read-Host 'Press Enter to close'
    exit 1
  }
  break
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
$SlotEffectValues = @{
  SFX = @("$FxGuid,13", "$FxGuid,5", "$FxGuid,1")
  MFX = @("$FxGuid,14", "$FxGuid,6")
  EFX = @("$FxGuid,15", "$FxGuid,7", "$FxGuid,2")
}
$ModesGuid = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$DefaultProcessingMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'
$ModeValues = @{ SFX = "$ModesGuid,5"; MFX = "$ModesGuid,6"; EFX = "$ModesGuid,7" }

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

function Restore-FxFromJson([string]$jsonFile) {
  $parsed = Get-Content $jsonFile -Raw -Encoding UTF8 | ConvertFrom-Json
  $flow = [string]$parsed.Flow
  $id = [string]$parsed.Id
  if (-not $flow -or -not $id) {
    throw "Invalid JSON backup (missing Flow or Id): $jsonFile"
  }
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  if (-not (Test-Path "HKLM:\$subKey")) {
    Write-Warning "Skipping backup for missing endpoint $flow $id ($($jsonFile | Split-Path -Leaf))"
    return $null
  }
  $origRecords = @($parsed.Values | ForEach-Object { $_ })
  $origNames = @($origRecords | ForEach-Object { [string]$_.Name })
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    foreach ($cur in @($key.GetValueNames())) {
      if ($origNames -notcontains $cur) { $key.DeleteValue($cur, $false) }
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
  Write-Host "Restored $flow $id from $($jsonFile | Split-Path -Leaf)"
  $id
}

if ($RestoreBackup -and -not (Test-Path $RestoreBackup)) { throw "Backup not found: $RestoreBackup" }

Stop-Service Audiosrv -Force
try {
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
          Write-Host "Removed from $flow $($ep.PSChildName) $name"
        }
        if ($removedAny -and -not $RestoreBackup) {
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
              Write-Host "Removed empty $slot mode $modeName from $flow $($ep.PSChildName)"
            }
          }
        }
      }
      finally { $regKey.Dispose() }
    }
  }

  if ($RestoreBackup) {
    if ((Get-Item $RestoreBackup -ErrorAction SilentlyContinue) -is [System.IO.DirectoryInfo]) {
      $restoredIds = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
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
        if ($rf.Name -match '^(\{[a-f0-9-]+\})-FxProperties-' -and $restoredIds.Contains($matches[1])) { continue }
        & reg.exe import $rf.FullName
        if ($LASTEXITCODE -ne 0) { Write-Warning "reg import failed ($($rf.Name), exit $LASTEXITCODE) - FxProperties subkey ACLs often block reg.exe import" }
        else { Write-Host "Restored $($rf.Name)" }
      }
    }
    elseif ($RestoreBackup -like '*.json') {
      $null = Restore-FxFromJson $RestoreBackup
    }
    else {
      & reg.exe import $RestoreBackup
      if ($LASTEXITCODE -ne 0) { Write-Warning "reg import failed (exit $LASTEXITCODE)" } else { Write-Host "Restored $RestoreBackup" }
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
  if ($RemoveBackups) {
    if (Test-Path $BackupDir) {
      Remove-Item $BackupDir -Recurse -Force
      Write-Host "Removed backups in $BackupDir"
    }
    $legacy = Get-Item $LegacyBackupRoot -Force -ErrorAction SilentlyContinue
    if ($legacy -and -not ($legacy.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      $legacyDir = Get-Item (Join-Path $LegacyBackupRoot 'apo-backup') -Force -ErrorAction SilentlyContinue
      if ($legacyDir -and -not ($legacyDir.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Get-ChildItem $legacyDir.FullName -File -Force -Filter '*-FxProperties-*' | Remove-Item -Force
        if (-not (Get-ChildItem $legacyDir.FullName -Force)) { Remove-Item $legacyDir.FullName -Force }
      }
      if (-not (Get-ChildItem $LegacyBackupRoot -Force)) { Remove-Item $LegacyBackupRoot -Force }
      Write-Host "Removed old backups in $LegacyBackupRoot"
    }
  }
}
finally {
  Start-Service Audiosrv
}

# The installer's notes for the app log registered this event source.
try { [System.Diagnostics.EventLog]::DeleteEventSource('GamersEqualizer') } catch { }

# The installed app started this and exits right after; its exe cannot go while it runs.
foreach ($file in $InstalledExe, "$InstalledExe.new") {
  for ($try = 0; $try -lt 20 -and (Test-Path $file); $try++) {
    Remove-Item $file -Force -ErrorAction SilentlyContinue
    if (Test-Path $file) { Start-Sleep -Seconds 1 }
  }
  if (Test-Path $file) { Write-Warning "Could not delete $file (still running?); delete it by hand." }
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
    Write-Host "Removed temporary scripts in $normDir"
  }
}

Write-Host 'Uninstalled.'
if ($FromApp) { Start-Sleep -Seconds 3 }
