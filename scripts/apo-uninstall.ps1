#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Removes the prototype GamersEqualizer capture APO everywhere it was added.

.DESCRIPTION
  Removes our CLSID from every capture/render endpoint's effect lists (other effects stay),
  unregisters the COM class and APO, deletes the config key and the installed DLL, and
  restarts the Windows Audio service.
  -RestoreBackup <file.reg> additionally re-imports an FxProperties backup made by apo-install.ps1
  (byte-for-byte original state of that endpoint).
#>
param([string]$RestoreBackup)

$ErrorActionPreference = 'Stop'

# Mic chain, output EQ and stream EQ (all in the same DLL).
$Clsids = @('{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}', '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}', '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}')
$InstallDir = Join-Path $env:ProgramFiles 'GamersEqualizer\apo'
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
        $rest = @(@($prop.Value) | Where-Object { $Clsids -notcontains $_.ToLower() })
        if ($rest.Count -eq @($prop.Value).Count) { continue }
        # An empty list deletes the value.
        Set-FxValue "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$($ep.PSChildName)\FxProperties" $prop.Name ([string[]]$rest)
        Write-Host "Removed from $flow $($ep.PSChildName) $($prop.Name)"
      }
    }
  }

  if ($RestoreBackup) {
    & reg.exe import $RestoreBackup
    if ($LASTEXITCODE -ne 0) { Write-Warning "reg import failed (exit $LASTEXITCODE)" } else { Write-Host "Restored $RestoreBackup" }
  }

  $keys = @('HKLM:\SOFTWARE\GamersEqualizer\Apo')
  foreach ($c in $Clsids) {
    $keys += "HKLM:\SOFTWARE\Classes\CLSID\$c", "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$c"
  }
  foreach ($key in $keys) {
    if (Test-Path $key) { Remove-Item $key -Recurse -Force }
  }
  if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force }
}
finally {
  Start-Service Audiosrv
}
Write-Host 'Uninstalled.'
