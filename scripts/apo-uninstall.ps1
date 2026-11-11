#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Removes GamersEqualizer APOs everywhere they were added.

.DESCRIPTION
  Removes our CLSIDs from every capture/render endpoint's effect lists (other effects stay
  and original String vs MultiString registry value kinds are preserved), unregisters the COM
  classes and APOs, deletes the config key, the installed DLL and the installed app (once it
  has exited), and restarts the Windows Audio service.
  -RestoreBackup <file.json | file.reg | dir> additionally restores exact pre-install
  FxProperties values from backups made by apo-install.ps1: JSON backups are restored directly
  via .NET RegistryKey (SetValue + QueryValues), avoiding the reg.exe import ACL failure on
  TrustedInstaller-owned FxProperties subkeys; legacy .reg files without JSON fallback still
  attempt reg.exe import.
  -RemoveBackups also deletes those backups (%ProgramFiles%\GamersEqualizer\apo-backup, and
  the .reg files older versions left in %ProgramData%\GamersEqualizer); the app's
  "Uninstall from Windows" button passes it.
#>
param(
  [string]$RestoreBackup,
  [switch]$RemoveBackups,
  # Started by the app: keep the window open on errors, briefly on success.
  [switch]$FromApp
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
        foreach ($name in @($regKey.GetValueNames())) {
          if ($name -notlike '{*') { continue }
          $kind = $regKey.GetValueKind($name)
          if ($kind -ne [Microsoft.Win32.RegistryValueKind]::String -and $kind -ne [Microsoft.Win32.RegistryValueKind]::MultiString) { continue }
          $raw = $regKey.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
          $vals = @($raw | Where-Object { $null -ne $_ })
          $rest = @($vals | Where-Object { $Clsids -notcontains $_ })
          if ($rest.Count -eq $vals.Count) { continue }
          Set-FxValue $subKey $name ([string[]]$rest) $kind
          Write-Host "Removed from $flow $($ep.PSChildName) $name"
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
Write-Host 'Uninstalled.'
if ($FromApp) { Start-Sleep -Seconds 3 }
