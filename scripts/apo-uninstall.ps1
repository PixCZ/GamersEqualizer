#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Removes the prototype GamersEqualizer capture APO everywhere it was added.

.DESCRIPTION
  Removes our CLSID from every capture/render endpoint's effect lists (other effects stay),
  unregisters the COM class and APO, deletes the config key, the installed DLL and the
  installed app (once it has exited), and restarts the Windows Audio service.
  -RestoreBackup <file.reg | dir> additionally re-imports an FxProperties backup made by apo-install.ps1
  (byte-for-byte original state of endpoint(s); pass a file or the apo-backup directory).
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

# FxProperties grants Administrators only SetValue + read; the registry provider asks for more
# and gets "access denied", so open the key with exactly these rights.
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

if ($RestoreBackup -and -not (Test-Path $RestoreBackup)) { throw "Backup not found: $RestoreBackup" }

Stop-Service Audiosrv -Force
try {
  foreach ($flow in 'Capture', 'Render') {
    foreach ($ep in Get-ChildItem (Join-Path $MmDevices $flow)) {
      $fxKey = Join-Path $ep.PSPath 'FxProperties'
      if (-not (Test-Path $fxKey)) { continue }
      $fx = Get-ItemProperty $fxKey
      foreach ($prop in $fx.PSObject.Properties) {
        if ($prop.Name -notlike '{*') { continue }
        # Drivers (Realtek, ...) also keep REG_DWORD / REG_BINARY values here: only string lists
        # can hold our CLSIDs. `-notcontains` already ignores case.
        if ($prop.Value -isnot [string] -and $prop.Value -isnot [string[]]) { continue }
        $rest = @(@($prop.Value) | Where-Object { $Clsids -notcontains $_ })
        if ($rest.Count -eq @($prop.Value).Count) { continue }
        # An empty list deletes the value.
        Set-FxValue "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$($ep.PSChildName)\FxProperties" $prop.Name ([string[]]$rest)
        Write-Host "Removed from $flow $($ep.PSChildName) $($prop.Name)"
      }
    }
  }

  if ($RestoreBackup) {
    if ((Get-Item $RestoreBackup -ErrorAction SilentlyContinue) -is [System.IO.DirectoryInfo]) {
      $files = @(Get-ChildItem -Path $RestoreBackup -Filter '*-FxProperties-original.reg' -ErrorAction SilentlyContinue)
      if ($files.Count -eq 0) { $files = @(Get-ChildItem -Path $RestoreBackup -Filter '*.reg' -ErrorAction SilentlyContinue) }
      foreach ($f in $files) {
        & reg.exe import $f.FullName
        if ($LASTEXITCODE -ne 0) { Write-Warning "reg import failed ($($f.Name), exit $LASTEXITCODE)" } else { Write-Host "Restored $($f.Name)" }
      }
    } else {
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
        Get-ChildItem $legacyDir.FullName -File -Force -Filter '*-FxProperties-*.reg' | Remove-Item -Force
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
