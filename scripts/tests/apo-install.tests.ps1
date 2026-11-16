# Self-check of the pure helpers in apo-install.ps1. Touches nothing; runs anywhere PowerShell
# does (also `pwsh` on macOS / Linux):  pwsh scripts/tests/apo-install.tests.ps1
$ErrorActionPreference = 'Stop'
# apo-common.ps1 is what the app puts in place of the script's `# @common` line.
$scripts = 'apo-common.ps1', 'apo-install.ps1' | ForEach-Object { Join-Path (Split-Path $PSScriptRoot) $_ }
# The script installs when it is run, so only the pieces under test are taken out of it.
$functions = 'Get-EndpointBlocker', 'Read-FxList', 'Test-ApoNoteStarted', 'Get-Targets', 'Test-SlotHadDriverEffects',
  'ConvertTo-FxJson', 'Get-MigrationSlots', 'Resolve-ProbeOpen', 'Restore-FxSnapshot', 'Test-BackupCreated',
  'Remove-OurEffects'
$variables = '$FxGuid', '$NullGuid', '$Classes', '$CompositeValues', '$LegacyValues', '$RestoreKeySource'
$pieces = @(foreach ($script in $scripts) {
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$null, [ref]$null)
    $ast.FindAll({
        param($n)
        ($n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -in $functions) -or
        ($n -is [System.Management.Automation.Language.AssignmentStatementAst] -and $n.Left.Extent.Text -in $variables)
      }, $false)
  })
$expected = $functions.Count + $variables.Count
if ($pieces.Count -ne $expected) { throw "Expected $expected functions and variables in $($scripts -join ', '); found $($pieces.Count)." }
$pieces | ForEach-Object { Invoke-Expression $_.Extent.Text }

$failed = 0
$total = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  $script:total++
  if (-not $ok) {
    $script:failed++
    Write-Host "FAIL $name $detail" -ForegroundColor Red
  }
}

# --- Get-EndpointBlocker --------------------------------------------------------------------
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
foreach ($case in $cases) {
  $blocker = Get-EndpointBlocker $case.Fx
  Check "blocker: $($case.Name)" ([bool]$blocker -eq $case.Blocked) "expected blocked=$($case.Blocked), got '$blocker'"
}

# --- Read-FxList: REG_SZ or REG_MULTI_SZ in, a list without blanks and case-insensitive dupes --
$list = Read-FxList @('  {A}', '', $null, '{a}', '{B} ')
Check 'Read-FxList trims, drops blanks and dupes' (($list -join ',') -eq '{A},{B}') "got '$($list -join ',')'"
$list = Read-FxList '{C}'
Check 'Read-FxList takes a single REG_SZ' ($list.Count -eq 1 -and $list[0] -eq '{C}') "got $($list.Count)"
$list = Read-FxList $null
Check 'Read-FxList of nothing is an empty list, not $null' ($null -ne $list -and $list.Count -eq 0) ''
$list.Add('{D}')
Check 'Read-FxList result can be added to' ($list.Count -eq 1) ''

# --- Get-Targets ------------------------------------------------------------------------------
$t = Get-Targets 'Capture' 'MFX' $false
Check 'Get-Targets capture' ($t.Count -eq 1 -and $t[0][0] -eq 'Capture' -and $t[0][1] -eq 'MFX') "got $($t | ConvertTo-Json -Compress)"
$t = Get-Targets 'Render' 'EFX' $false
Check 'Get-Targets render: output + stream EQ' ($t.Count -eq 2 -and $t[0][1] -eq 'EFX' -and $t[1][0] -eq 'Stream' -and $t[1][1] -eq 'SFX') "got $($t | ConvertTo-Json -Compress)"
$t = Get-Targets 'Render' 'EFX' $true
Check 'Get-Targets render -NoStreamEq' ($t.Count -eq 1 -and $t[0][0] -eq 'Render') "got $($t | ConvertTo-Json -Compress)"
$t = Get-Targets 'Render' 'SFX' $false
Check 'Get-Targets render SFX: stream EQ only' ($t.Count -eq 1 -and $t[0][0] -eq 'Stream') "got $($t | ConvertTo-Json -Compress)"
$threw = $false
try { $null = Get-Targets 'Capture' 'SFX' $false } catch { $threw = $true }
Check 'Get-Targets capture SFX is refused' $threw ''

# --- Test-ApoNoteStarted ----------------------------------------------------------------------
$guid = '{cf0a2270-1885-4dd6-82a5-f636e9efadd5}'
$other = '{b45cef56-a56b-49b0-88c0-4af057026718}'
$mic = 'GamersEqualizer Mic Chain'
$tail = '(stream |refused a stream)'
function Res($notes, $pre) { [pscustomobject]@{ Notes = @($notes); PreNotes = @($pre) } }
$noteCases = @(
  @{ Name = 'tagged with this endpoint';               Res = (Res "audiodg.exe pid 1: $mic [$guid]: stream 48000 Hz" @());  Only = $false; Started = $true }
  @{ Name = 'tagged, before the probe';                Res = (Res @() "audiodg.exe pid 1: $mic [$guid]: stream 48000 Hz");  Only = $false; Started = $true }
  @{ Name = 'tagged with another endpoint';            Res = (Res "audiodg.exe pid 1: $mic [$other]: stream 48000 Hz" @()); Only = $true;  Started = $false }
  @{ Name = 'untagged, only device of its flow';       Res = (Res "audiodg.exe pid 1: $mic`: stream 48000 Hz" @());         Only = $true;  Started = $true }
  @{ Name = 'untagged, several devices of its flow';   Res = (Res "audiodg.exe pid 1: $mic`: stream 48000 Hz" @());         Only = $false; Started = $false }
  @{ Name = 'untagged before the probe, only device';  Res = (Res @() "audiodg.exe pid 1: $mic`: refused a stream");         Only = $true;  Started = $true }
  @{ Name = 'untagged before, tags exist elsewhere';   Res = (Res "x: $mic [$other]: class requested" "y: $mic`: stream 1"); Only = $true;  Started = $false }
  @{ Name = 'other class';                             Res = (Res "audiodg.exe pid 1: GamersEqualizer Output EQ [$guid]: stream 2 started" @()); Only = $true; Started = $false }
  @{ Name = 'nothing';                                 Res = (Res @() @());                                                 Only = $true;  Started = $false }
)
foreach ($case in $noteCases) {
  $started = Test-ApoNoteStarted $case.Res $mic $guid $tail $case.Only
  Check "notes: $($case.Name)" ($started -eq $case.Started) "expected $($case.Started), got $started"
}

# --- Test-SlotHadDriverEffects: Default mode only where the driver had no effects ------------
$ourMic = $Classes.Capture.Clsid
function Snap([hashtable]$values) {
  [pscustomobject]@{ Records = @($values.Keys | ForEach-Object { [pscustomobject]@{ Name = "$FxGuid,$_"; Value = $values[$_] } }) }
}
Check 'slot empty before' (-not (Test-SlotHadDriverEffects (Snap @{}) 'EFX')) ''
Check 'only ours before (an update)' (-not (Test-SlotHadDriverEffects (Snap @{ 15 = @($ourMic) }) 'EFX')) ''
Check 'null GUID only' (-not (Test-SlotHadDriverEffects (Snap @{ 15 = @($NullGuid) }) 'EFX')) ''
Check 'null GUID and ours' (-not (Test-SlotHadDriverEffects (Snap @{ 15 = @($NullGuid, $ourMic) }) 'EFX')) ''
Check 'driver effect in EFX' (Test-SlotHadDriverEffects (Snap @{ 15 = @($oem, $ourMic) }) 'EFX') ''
Check 'driver effect in another slot' (-not (Test-SlotHadDriverEffects (Snap @{ 14 = @($oem) }) 'EFX')) ''
Check 'no snapshot' (-not (Test-SlotHadDriverEffects $null 'EFX')) ''

# --- ConvertTo-FxJson: what apo-uninstall.ps1 reads back ---------------------------------------
$kinds = [Microsoft.Win32.RegistryValueKind]
$snapshot = [pscustomobject]@{
  Flow = 'Capture'; Id = $guid
  Records = @(
    [pscustomobject]@{ Name = "$FxGuid,15"; Kind = $kinds::MultiString; Value = [string[]]@($oem) }
    [pscustomobject]@{ Name = "$FxGuid,5"; Kind = $kinds::String; Value = $oem }
    [pscustomobject]@{ Name = 'bin'; Kind = $kinds::Binary; Value = [byte[]]@(1, 2, 255) }
    [pscustomobject]@{ Name = 'dw'; Kind = $kinds::DWord; Value = 7 }
  )
}
$back = ConvertTo-FxJson $snapshot | ConvertFrom-Json
$byName = @{}
$back.Values | ForEach-Object { $byName[$_.Name] = $_ }
Check 'json keeps flow and id' ($back.Flow -eq 'Capture' -and $back.Id -eq $guid) ''
Check 'json: one-entry multi-string stays a list' (@($byName["$FxGuid,15"].Value).Count -eq 1 -and $byName["$FxGuid,15"].Kind -eq 'MultiString' -and $byName["$FxGuid,15"].Value -is [array]) "got $($byName["$FxGuid,15"] | ConvertTo-Json -Compress)"
Check 'json: string' ($byName["$FxGuid,5"].Value -eq $oem -and $byName["$FxGuid,5"].Kind -eq 'String') ''
Check 'json: binary as base64' ((([Convert]::FromBase64String($byName['bin'].Value)) -join ',') -eq '1,2,255') ''
Check 'json: dword' ($byName['dw'].Value -eq 7 -and $byName['dw'].Kind -eq 'DWord') ''
Check 'json: an existing key is not marked created' ($back.Created -eq $false) ''
$made = ConvertTo-FxJson ([pscustomobject]@{ Flow = 'Capture'; Id = $guid; Records = @(); Created = $true }) | ConvertFrom-Json
Check 'json: a made key is marked created, no values' ($made.Created -eq $true -and @($made.Values).Count -eq 0) "got $($made | ConvertTo-Json -Compress)"

# --- Remove-OurEffects: an update's rollback target holds no effect of ours -------------------
$update = [pscustomobject]@{
  Flow = 'Render'; Id = $guid; Created = $true
  Records = @(
    [pscustomobject]@{ Name = "$FxGuid,15"; Kind = $kinds::MultiString; Value = [string[]]@($oem, $Classes.Render.Clsid.ToUpper()) }
    [pscustomobject]@{ Name = "$FxGuid,13"; Kind = $kinds::MultiString; Value = [string[]]@($Classes.Stream.Clsid) }
    [pscustomobject]@{ Name = "$FxGuid,14"; Kind = $kinds::MultiString; Value = [string[]]@() }
    [pscustomobject]@{ Name = "$FxGuid,5"; Kind = $kinds::String; Value = $oem }
  )
}
$target = Remove-OurEffects $update
$byName = @{}
$target.Records | ForEach-Object { $byName[$_.Name] = $_ }
Check 'rollback target: ours out, the driver effect stays' ((@($byName["$FxGuid,15"].Value) -join ',') -eq $oem) "got '$(@($byName["$FxGuid,15"].Value) -join ',')'"
Check 'rollback target: a list of ours only goes' (-not $byName.ContainsKey("$FxGuid,13")) ''
Check 'rollback target: the driver''s empty list stays' ($byName.ContainsKey("$FxGuid,14")) ''
Check 'rollback target: other values stay' ($byName["$FxGuid,5"].Value -eq $oem -and $target.Created -and $target.Id -eq $guid) ''
Check 'rollback target: the snapshot itself is unchanged' (@($update.Records).Count -eq 4 -and @($update.Records[0].Value).Count -eq 2) ''

# --- Made FxProperties keys -------------------------------------------------------------------
# Compiles here too (pwsh), so a C# error shows before the script reaches Windows; the
# registry calls themselves run only there.
Add-Type -TypeDefinition $RestoreKeySource
Check 'restore-key helper compiles' ([bool]('GamersEqualizer.RestoreKey' -as [type])) ''
$keyCalls = New-Object System.Collections.Generic.List[string]
function Remove-FxKey([string]$subKey) { $keyCalls.Add("remove $subKey") }
Restore-FxSnapshot ([pscustomobject]@{ Flow = 'Capture'; Id = '{id}'; Records = @(); Created = $true })
Check 'rollback of a made key deletes it' (($keyCalls -join '|') -eq 'remove SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture\{id}\FxProperties') "got '$($keyCalls -join '|')'"

# --- Test-BackupCreated: a key an earlier install made, read back from its backup -------------
$backupDir = Join-Path ([IO.Path]::GetTempPath()) "ge-apo-tests-$PID"
New-Item -ItemType Directory -Force $backupDir | Out-Null
try {
  $backup = Join-Path $backupDir 'backup.json'
  Set-Content $backup (ConvertTo-FxJson ([pscustomobject]@{ Flow = 'Capture'; Id = $guid; Records = @(); Created = $true }))
  Check 'backup of a made key' (Test-BackupCreated $backup) ''
  Set-Content $backup (ConvertTo-FxJson $snapshot)
  Check 'backup of an existing key' (-not (Test-BackupCreated $backup)) ''
  Set-Content $backup '{ not json'
  Check 'corrupt backup' (-not (Test-BackupCreated $backup)) ''
}
finally { Remove-Item -Recurse -Force $backupDir }

# --- Get-MigrationSlots ---------------------------------------------------------------------
$migrationCases = @(
  @{ Name = 'mic in EFX';                      Fx = (Fx @{ 6 = $oem });              Slots = @('EFX');         Want = 'EFX' }
  @{ Name = 'output: EFX + stream SFX';        Fx = (Fx @{ 5 = $oem; 6 = $oem });    Slots = @('EFX', 'SFX');  Want = 'EFX,SFX' }
  @{ Name = 'old pre-mix LFX adds SFX';        Fx = (Fx @{ 1 = $oem });              Slots = @('EFX');         Want = 'EFX,SFX' }
  @{ Name = 'null LFX adds nothing';           Fx = (Fx @{ 1 = $NullGuid });         Slots = @('MFX');         Want = 'MFX' }
  @{ Name = 'no FxProperties values';          Fx = $null;                           Slots = @('EFX', 'EFX');  Want = 'EFX' }
)
foreach ($case in $migrationCases) {
  $got = (Get-MigrationSlots $case.Fx $case.Slots) -join ','
  Check "migration slots: $($case.Name)" ($got -eq $case.Want) "expected '$($case.Want)', got '$got'"
}

# --- Resolve-ProbeOpen (registry, audio and probe replaced by stand-ins) ----------------------
# Only the line the app reads matters here (apo_install::rolled_back_endpoint).
function Say([string]$text, [string]$color) { if ($text -match 'rollback-line') { $script:calls.Add('rollback-line') } }
function Get-RollbackLine($job) { 'rollback-line' }
function Rollback-Job($job, [datetime]$since) { $script:calls.Add('rollback'); $script:t1 }
function Set-JobFx($snapshot) {
  $script:calls.Add("set $($snapshot.Tag)")
  if ($snapshot.Tag -eq 'installed' -and $script:restoreFails) { throw 'restore failed' }
  $script:t1
}
function Get-FxSnapshot([string]$flow, [string]$id) {
  if ($script:readFails) { throw 'access denied' }
  [pscustomobject]@{ Tag = 'installed' }
}
function Invoke-EndpointProbe($job, [datetime]$since) { $script:calls.Add('probe'); $script:baseline }
$t0 = Get-Date '2026-11-12T10:00:00'
$t1 = Get-Date '2026-11-12T10:00:05'
$openCases = @(
  @{ Name = 'audiodg crashed';               Crashed = $true;  Migrated = $false; Snap = $true;  Read = $true;  Base = 'fails'; Want = 'rollback' }
  @{ Name = 'check did not run';             NotRun = $true;   Migrated = $false; Snap = $true;  Read = $true;  Base = 'fails'; Want = 'rollback' }
  @{ Name = 'baseline check did not run';    Crashed = $false; Migrated = $false; Snap = $true;  Read = $true;  Base = 'notrun'; Want = 'set before|probe|rollback-line' }
  @{ Name = 'putting ours back fails';       Crashed = $false; Migrated = $false; Snap = $true;  Read = $true;  Base = 'fails'; Want = 'set before|probe|set installed|rollback-line'; RestoreFails = $true }
  @{ Name = 'driver effects were moved';     Crashed = $false; Migrated = $true;  Snap = $true;  Read = $true;  Base = 'fails'; Want = 'rollback' }
  @{ Name = 'no pre-install snapshot';       Crashed = $false; Migrated = $false; Snap = $false; Read = $true;  Base = 'fails'; Want = 'rollback' }
  @{ Name = 'settings unreadable';           Crashed = $false; Migrated = $false; Snap = $true;  Read = $false; Base = 'fails'; Want = 'rollback' }
  @{ Name = 'opens without ours: stays out'; Crashed = $false; Migrated = $false; Snap = $true;  Read = $true;  Base = 'opens'; Want = 'set before|probe|rollback-line' }
  @{ Name = 'crash without ours: stays out'; Crashed = $false; Migrated = $false; Snap = $true;  Read = $true;  Base = 'crash'; Want = 'set before|probe|rollback-line' }
  @{ Name = 'fails without ours: back in';   Crashed = $false; Migrated = $false; Snap = $true;  Read = $true;  Base = 'fails'; Want = 'set before|probe|set installed' }
)
foreach ($case in $openCases) {
  $calls = New-Object System.Collections.Generic.List[string]
  $readFails = -not $case.Read
  $restoreFails = [bool]$case.RestoreFails
  $baseline = [pscustomobject]@{ OpenOk = $case.Base -eq 'opens'; Crashed = $case.Base -eq 'crash' }
  if ($case.Base -eq 'notrun') { $baseline | Add-Member Ran $false }
  $FxSnapshots = @{}
  if ($case.Snap) { $FxSnapshots['Capture:{id}'] = [pscustomobject]@{ Tag = 'before' } }
  $job = [pscustomobject]@{ Flow = 'Capture'; Id = '{id}'; Word = 'microphone'; Migrated = $case.Migrated }
  $res = [pscustomobject]@{ OpenOk = $false; Crashed = [bool]$case.Crashed }
  if ($case.NotRun) { $res | Add-Member Ran $false }
  $threw = $false
  try { $started = Resolve-ProbeOpen $job $res $t0 } catch { $threw = $true; $started = $t1 }
  $got = $calls -join '|'
  $ok = $got -eq $case.Want -and $started -eq $t1 -and $threw -eq [bool]$case.RestoreFails
  Check "probe open: $($case.Name)" $ok "expected '$($case.Want)', got '$got' (started $started, threw $threw)"
}

if ($failed) { throw "$failed of $total checks failed." }
Write-Host "apo-install: $total checks passed."
