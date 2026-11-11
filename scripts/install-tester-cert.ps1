#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Installs or uninstalls the GamersEqualizer development certificate (delegates to packaging\install-tester-cert.ps1).
#>
& (Join-Path $PSScriptRoot '..\packaging\install-tester-cert.ps1') @args
