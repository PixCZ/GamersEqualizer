# Self-check of the pure helpers in apo-install.ps1. Touches nothing; runs anywhere PowerShell
# does (also `pwsh` on macOS / Linux):  pwsh scripts/tests/apo-install.tests.ps1
$ErrorActionPreference = 'Stop'
$script = Join-Path (Split-Path $PSScriptRoot) 'apo-install.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$null, [ref]$null)
# The script installs when it is run, so only the pieces under test are taken out of it.
$pieces = $ast.FindAll({
    param($n)
    ($n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Get-EndpointBlocker') -or
    ($n -is [System.Management.Automation.Language.AssignmentStatementAst] -and $n.Left.Extent.Text -in '$FxGuid', '$NullGuid')
  }, $false)
if ($pieces.Count -ne 3) { throw "Expected Get-EndpointBlocker, `$FxGuid and `$NullGuid in $script; found $($pieces.Count)." }
$pieces | ForEach-Object { Invoke-Expression $_.Extent.Text }

function Fx([hashtable]$values) {
  $named = @{}
  foreach ($k in $values.Keys) { $named["$FxGuid,$k"] = $values[$k] }
  [pscustomobject]$named
}
$oem = '{11111111-2222-3333-4444-555555555555}'
$cases = @(
  @{ Name = 'no values at all';                     Fx = $null;                                       Blocked = $false }
  @{ Name = 'composite lists only (Realtek UAD)';   Fx = (Fx @{ 13 = @($oem); 15 = @($oem) });        Blocked = $false }
  @{ Name = 'legacy SFX/MFX (USB audio)';           Fx = (Fx @{ 5 = $oem; 6 = $oem });                Blocked = $false }
  @{ Name = 'LFX only: migrated';                   Fx = (Fx @{ 1 = $oem });                          Blocked = $false }
  @{ Name = 'LFX + GFX only: GFX would be lost';    Fx = (Fx @{ 1 = $oem; 2 = $oem });                Blocked = $true }
  @{ Name = 'GFX only';                             Fx = (Fx @{ 2 = $oem });                          Blocked = $true }
  @{ Name = 'null GFX';                             Fx = (Fx @{ 2 = $NullGuid });                     Blocked = $false }
  @{ Name = 'GFX next to a mode effect';            Fx = (Fx @{ 2 = $oem; 6 = $oem });                Blocked = $false }
  @{ Name = 'GFX next to our composite list';       Fx = (Fx @{ 2 = $oem; 15 = @($oem) });            Blocked = $false }
)
$failed = 0
foreach ($case in $cases) {
  $blocker = Get-EndpointBlocker $case.Fx
  if ([bool]$blocker -ne $case.Blocked) {
    $failed++
    Write-Host "FAIL $($case.Name): expected blocked=$($case.Blocked), got '$blocker'" -ForegroundColor Red
  }
}
if ($failed) { throw "$failed of $($cases.Count) cases failed." }
Write-Host "apo-install: $($cases.Count) cases passed."
