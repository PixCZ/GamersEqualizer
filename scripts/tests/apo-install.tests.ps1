# Self-check of the pure helpers in apo-install.ps1. Touches nothing; runs anywhere PowerShell
# does (also `pwsh` on macOS / Linux):  pwsh scripts/tests/apo-install.tests.ps1
$ErrorActionPreference = 'Stop'
# apo-common.ps1 is what the app puts in place of the script's `# @common` line.
$scripts = 'apo-common.ps1', 'apo-install.ps1' | ForEach-Object { Join-Path (Split-Path $PSScriptRoot) $_ }
# The script installs when it is run, so only the pieces under test are taken out of it.
$functions = 'Get-EndpointBlocker', 'Read-FxList', 'Test-ApoNoteStarted', 'Get-Targets', 'Test-SlotHadDriverEffects',
  'ConvertTo-FxJson', 'Get-MigrationSlots', 'Resolve-ProbeOpen', 'Restore-FxSnapshot', 'Test-BackupCreated',
  'Remove-OurEffects', 'Restore-InstallStart', 'Get-RollbackTarget', 'Set-DoesNotOpen', 'Select-OpenableJobs',
  'Install-AppFiles', 'Install-WithoutDevices'
$variables = '$FxGuid', '$NullGuid', '$DefaultProcessingMode', '$Classes', '$CompositeValues', '$LegacyValues', '$RestoreKeySource',
  '$ProbeExitDeviceInvalidated', '$ProbeTimeoutMs'
$parsed = @{}
# A script that does not parse would fail first on a user's PC, elevated. The uninstaller has no
# pieces under test here, so it is at least parsed. Windows PowerShell 5.1 runs them there: the
# gate runs this file under it too (scripts\check.ps1), so its parser has the last word.
foreach ($script in @($scripts) + (Join-Path (Split-Path $PSScriptRoot) 'apo-uninstall.ps1')) {
  $parseErrors = $null
  $parsed[$script] = [System.Management.Automation.Language.Parser]::ParseFile($script, [ref]$null, [ref]$parseErrors)
  if ($parseErrors) {
    throw "$script does not parse: $($parseErrors[0].Message) (line $($parseErrors[0].Extent.StartLineNumber))"
  }
}
$pieces = @(foreach ($script in $scripts) {
    $ast = $parsed[$script]
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

# --- Get-RollbackTarget: a key an earlier install made goes with the update's rollback --------
# Left in place, the next install would back it up as the driver's own and no uninstall delete it.
function MadeKey([string[]]$effects) {
  [pscustomobject]@{
    Flow = 'Capture'; Id = $guid
    Records = @(
      [pscustomobject]@{ Name = "$FxGuid,15"; Kind = $kinds::MultiString; Value = $effects }
      [pscustomobject]@{ Name = 'modes'; Kind = $kinds::MultiString; Value = [string[]]@($DefaultProcessingMode) }
    )
  }
}
Check 'rollback target: a made key only ours used goes' ([bool](Get-RollbackTarget (MadeKey @($ourMic)) $true).Created) ''
$shared = Get-RollbackTarget (MadeKey @($oem, $ourMic)) $true
Check 'rollback target: a made key other effects use now stays' (-not $shared.Created -and (@($shared.Records[0].Value) -join ',') -eq $oem) ''
Check 'rollback target: the driver''s own key stays' (-not (Get-RollbackTarget (MadeKey @($ourMic)) $false).Created) ''
# The backup still says "made", but ours are gone: the key may be one the driver made since.
Check 'rollback target: a key without ours stays' (-not (Get-RollbackTarget (MadeKey @()) $true).Created) ''

# --- Made FxProperties keys -------------------------------------------------------------------
# Compiles here too (pwsh), so a C# error shows before the script reaches Windows; the
# registry calls themselves run only there.
Add-Type -TypeDefinition $RestoreKeySource
Check 'restore-key helper compiles' ([bool]('GamersEqualizer.RestoreKey' -as [type])) ''
$keyCalls = New-Object System.Collections.Generic.List[string]
function Remove-FxKey([string]$subKey) { $keyCalls.Add("remove $subKey") }
Restore-FxSnapshot ([pscustomobject]@{ Flow = 'Capture'; Id = '{id}'; Records = @(); Created = $true })
Check 'rollback of a made key deletes it' (($keyCalls -join '|') -eq 'remove SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture\{id}\FxProperties') "got '$($keyCalls -join '|')'"

# --- Restore-InstallStart: a failed install puts back what the run found ----------------------
# Not the rollback target: an update that fails before it changes anything must leave ours in.
$restored = New-Object System.Collections.Generic.List[object]
function Restore-FxSnapshot($snapshot) { $restored.Add($snapshot) }
$FxSnapshots = @{ 'Render:{id}' = $update }
$RollbackTargets = @{ 'Render:{id}' = $target }
Restore-InstallStart
Check 'failed update: the endpoint keeps the effects it had' ($restored.Count -eq 1 -and [object]::ReferenceEquals($restored[0], $update)) "restored $($restored.Count)"

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
function Say([string]$text, [string]$color) {
  if ($text -match 'rollback-line') { $script:calls.Add('rollback-line') }
  if ($color) { $script:warned = $true }
}
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
  # Windows reports the device invalidated (Realtek Digital Output switched off in the driver): no
  # second probe, no audio restart, ours stay in, one neutral line.
  @{ Name = 'invalidated: no retry';         Inv = $true;      Migrated = $false; Snap = $true;  Read = $true;  Base = 'opens'; Want = ''; DoesNotOpen = $true; NoRestart = $true }
  @{ Name = 'invalidated, drivers moved';    Inv = $true;      Migrated = $true;  Snap = $true;  Read = $true;  Base = 'opens'; Want = ''; DoesNotOpen = $true; NoRestart = $true }
  @{ Name = 'other failure, invalidated without ours'; Migrated = $false; Snap = $true; Read = $true; Base = 'invalid'; Want = 'set before|probe|set installed' }
)
foreach ($case in $openCases) {
  $calls = New-Object System.Collections.Generic.List[string]
  $readFails = -not $case.Read
  $restoreFails = [bool]$case.RestoreFails
  $warned = $false
  $baseline = [pscustomobject]@{ OpenOk = $case.Base -eq 'opens'; Crashed = $case.Base -eq 'crash'; Invalidated = $case.Base -eq 'invalid' }
  if ($case.Base -eq 'notrun') { $baseline | Add-Member Ran $false }
  $RollbackTargets = @{}
  if ($case.Snap) { $RollbackTargets['Capture:{id}'] = [pscustomobject]@{ Tag = 'before' } }
  $job = [pscustomobject]@{ Flow = 'Capture'; Id = '{id}'; Word = 'microphone'; Migrated = $case.Migrated; DoesNotOpen = $false }
  $res = [pscustomobject]@{ OpenOk = $false; Crashed = [bool]$case.Crashed; Invalidated = [bool]$case.Inv }
  if ($case.NotRun) { $res | Add-Member Ran $false }
  $threw = $false
  try { $started = Resolve-ProbeOpen $job $res $t0 } catch { $threw = $true; $started = $t1 }
  $got = $calls -join '|'
  $wantStarted = if ($case.NoRestart) { $t0 } else { $t1 }
  $ok = $got -eq $case.Want -and $started -eq $wantStarted -and $threw -eq [bool]$case.RestoreFails -and
    $job.DoesNotOpen -eq [bool]$case.DoesNotOpen -and (-not $case.DoesNotOpen -or -not $warned)
  Check "probe open: $($case.Name)" $ok "expected '$($case.Want)', got '$got' (started $started, threw $threw, does not open $($job.DoesNotOpen), warned $warned)"
}

# --- Select-OpenableJobs: the check before installing (probe replaced by a stand-in) ------------
# Only exit 2 drops a device. Everything that could change an endpoint records 'changed': the check
# must call none of it.
foreach ($f in 'Stop-Audio', 'Start-Audio', 'Set-JobFx', 'Set-FxValue', 'New-FxKey', 'Restore-FxSnapshot', 'Migrate-LegacySlots', 'Set-EffectSlot') {
  Set-Item "function:$f" { $script:calls.Add('changed') }
}
function Say([string]$text, [string]$color) {
  $script:said.Add($text)
  if ($color) { $script:warned = $true }
}
# Exit codes of the stand-in probe processes; '{hangs}' never exits.
$probeExit = @{ '{never}' = $ProbeExitDeviceInvalidated; '{opens}' = 0; '{fails}' = 1; '{hangs}' = $ProbeExitDeviceInvalidated }
$crashes = @()
function Get-AudiodgCrashesSince([datetime]$since) { , @($script:crashes) }
function Start-EndpointProbe($job, [switch]$OpenOnly) {
  $script:calls.Add("start $($job.Id) open-only=$OpenOnly")
  $probe = [pscustomobject]@{ Id = $job.Id; ExitCode = $probeExit[$job.Id] }
  $probe | Add-Member ScriptMethod WaitForExit { param($ms) $script:calls.Add("wait $($this.Id)"); $this.Id -ne '{hangs}' }
  $probe | Add-Member ScriptMethod Kill { $script:calls.Add("kill $($this.Id)") }
  $probe
}
function TestJob([string]$id) { [pscustomobject]@{ Flow = 'Render'; Id = $id; Name = "dev $id"; Word = 'output device'; DoesNotOpen = $false } }
function Reset-Check {
  $script:calls = New-Object System.Collections.Generic.List[string]
  $script:said = New-Object System.Collections.Generic.List[string]
  $script:warned = $false
}

$ProbeCopy = 'probe.exe'
Reset-Check
$never, $opens, $fails, $hangs = (TestJob '{never}'), (TestJob '{opens}'), (TestJob '{fails}'), (TestJob '{hangs}')
$kept = Select-OpenableJobs @($never, $opens, $fails, $hangs)
$keptIds = @($kept | ForEach-Object { $_.Id }) -join ','
$neutral = @($said | Where-Object { $_ -match 'nothing to check' })
Check 'pre-check: exit 2 is dropped, exit 0, 1 and a hung check stay' ($keptIds -eq '{opens},{fails},{hangs}') "kept '$keptIds'"
Check 'pre-check: only the dropped device does not open' ($never.DoesNotOpen -and -not $opens.DoesNotOpen -and -not $fails.DoesNotOpen -and -not $hangs.DoesNotOpen) ''
$wantCalls = 'start {never} open-only=True|start {opens} open-only=True|start {fails} open-only=True|start {hangs} open-only=True|' +
  'wait {never}|wait {opens}|wait {fails}|wait {hangs}|kill {hangs}'
Check 'pre-check: nothing changed, all open-only probes start before the first wait' (($calls -join '|') -eq $wantCalls) "got '$($calls -join '|')'"
Check 'pre-check: a hung check is named' (@($said | Where-Object { $_ -match 'did not finish in time' }).Count -eq 1) "said '$($said -join '|')'"
# No `[Render:{guid}]`: the app would read it as rolled back (apo_install::rolled_back_endpoint).
Check 'pre-check: one neutral line, no warning' ($neutral.Count -eq 1 -and $neutral[0] -match 'leaves it as it is' -and $neutral[0] -notmatch '\[Render:' -and -not $warned) "said '$($said -join '|')', warned $warned"
$null = Resolve-ProbeOpen $never ([pscustomobject]@{ OpenOk = $false; Invalidated = $true }) $t0
Check 'pre-check: the line is not said twice for one device' (@($said | Where-Object { $_ -match 'nothing to check' }).Count -eq 1) "said '$($said -join '|')'"

Reset-Check
$none = Select-OpenableJobs @((TestJob '{never}'))
Check 'pre-check: none left' (@($none).Count -eq 0 -and $calls.Count -eq 2) "kept $(@($none).Count), calls '$($calls -join '|')'"

# An audiodg crash during the check: exit 2 may be its effect, so nothing is dropped.
Reset-Check
$crashes = @('crash')
$crashJob = TestJob '{never}'
$kept = Select-OpenableJobs @($crashJob)
Check 'pre-check: after an audiodg crash nothing is dropped' (@($kept).Count -eq 1 -and -not $crashJob.DoesNotOpen) "kept $(@($kept).Count)"
$crashes = @()

$ProbeCopy = $null
Reset-Check
$unprobed = TestJob '{never}'
$kept = Select-OpenableJobs @($unprobed, (TestJob '{opens}'))
Check 'pre-check: without a probe exe nothing is probed or dropped' (@($kept).Count -eq 2 -and $calls.Count -eq 0 -and -not $unprobed.DoesNotOpen -and $said.Count -eq 0) "kept $(@($kept).Count), calls '$($calls -join '|')'"

# --- Install-WithoutDevices: every device dropped, the shared DLL and app still go in -----------
# Real files in a temp folder; the endpoint-changing stand-ins above still record 'changed'.
function Register-App { $script:calls.Add('register') }
function Unblock-File($path) { }
function Get-Process { }
function Start-Sleep { }
# Windows audio around the swap; endpoint changes still record 'changed'.
function Stop-Audio { $script:calls.Add('stop-audio') }
function Start-Audio { $script:calls.Add('start-audio') }
$appDir = Join-Path ([IO.Path]::GetTempPath()) "ge-apo-app-tests-$PID"
New-Item -ItemType Directory -Force $appDir | Out-Null
try {
  $InstallDir = Join-Path $appDir 'installed'
  $InstalledDll = Join-Path $InstallDir 'gamersequalizer_apo.dll'
  $InstalledExe = Join-Path $InstallDir 'gamersequalizer.exe'
  $ProbeTemp = Join-Path $appDir 'probe.exe'
  $SourceDll = Join-Path $appDir 'new.dll'
  $AppExe = Join-Path $appDir 'new.exe'
  Set-Content $SourceDll 'dll v2'
  Set-Content $AppExe 'exe v2'
  $SourceDllSha256 = (Get-FileHash $SourceDll -Algorithm SHA256).Hash
  $AppExeSha256 = (Get-FileHash $AppExe -Algorithm SHA256).Hash
  # The app compares file times (apo_install::check_flow): the installed copies must keep the source's.
  $stamp = Get-Date '2026-11-20T08:00:00'
  (Get-Item $SourceDll).LastWriteTime = $stamp
  (Get-Item $AppExe).LastWriteTime = $stamp

  $NoAppInstall = $false
  Set-Content $ProbeTemp 'probe'
  Reset-Check
  Install-WithoutDevices @()
  # Copy-Item keeps the time on Windows (CopyFile), where the app runs; not asserted elsewhere.
  $timesKept = { param($path) $IsWindows -eq $false -or (Get-Item $path).LastWriteTime -eq $stamp }
  $installedDllOk = (Get-Content $InstalledDll) -eq 'dll v2' -and (& $timesKept $InstalledDll)
  $installedExeOk = (Get-Content $InstalledExe) -eq 'exe v2' -and (& $timesKept $InstalledExe)
  Check 'all dropped: DLL and app installed with the source file times' ($installedDllOk -and $installedExeOk) ''
  Check 'all dropped: entry made, no endpoint or audio change' (($calls -join '|') -eq 'register') "got '$($calls -join '|')'"
  Check 'all dropped: one neutral line, no warning, probe copy gone' (($said -join '|') -eq 'The effects were updated; no device could be changed.' -and -not $warned -and -not (Test-Path $ProbeTemp)) "said '$($said -join '|')', warned $warned"

  # A developer's build runs in place: the DLL only.
  Remove-Item -Recurse -Force $InstallDir
  $NoAppInstall = $true
  Reset-Check
  Install-WithoutDevices @()
  Check 'all dropped, developer build: DLL only, no entry' ((Test-Path $InstalledDll) -and -not (Test-Path $InstalledExe) -and $calls.Count -eq 0) "got '$($calls -join '|')'"

  # An installed DLL may be loaded in audiodg: audio is stopped around the swap and started again.
  $NoAppInstall = $false
  Set-Content $SourceDll 'dll v3'
  $SourceDllSha256 = (Get-FileHash $SourceDll -Algorithm SHA256).Hash
  Reset-Check
  Install-WithoutDevices @()
  Check 'all dropped over an installed DLL: replaced with audio stopped, then started' ((Get-Content $InstalledDll) -eq 'dll v3' -and ($calls -join '|') -eq 'stop-audio|start-audio|register') "got '$($calls -join '|')'"

  # A swap that keeps failing (the DLL stays held) fails the run, leaves the installed DLL whole and
  # still starts audio again.
  Set-Content $SourceDll 'dll v4'
  $SourceDllSha256 = (Get-FileHash $SourceDll -Algorithm SHA256).Hash
  function Move-Item { $script:calls.Add('move'); throw 'Cannot create a file when that file already exists.' }
  Reset-Check
  $threw = $false
  try { Install-WithoutDevices @() } catch { $threw = $_ -match 'could not be replaced' }
  Remove-Item function:Move-Item
  $moves = @($calls | Where-Object { $_ -eq 'move' }).Count
  $audio = ($calls | Where-Object { $_ -ne 'move' }) -join '|'
  Check 'all dropped, DLL held: fails after retries, old DLL whole, audio started again' ($threw -and $moves -eq 10 -and $audio -eq 'stop-audio|start-audio' -and (Get-Content $InstalledDll) -eq 'dll v3') "threw $threw, moves $moves, got '$audio'"

  # A device skipped for its driver effects keeps the run a failure, and nothing is installed.
  Remove-Item -Recurse -Force $InstallDir
  $NoAppInstall = $false
  Reset-Check
  $threw = $false
  try { Install-WithoutDevices @('Skipped dev: GFX.') } catch { $threw = $_ -match '^Skipped dev: GFX\. Nothing was changed\.$' }
  Check 'all dropped next to a skipped device: fails, nothing installed' ($threw -and -not (Test-Path $InstallDir) -and $calls.Count -eq 0) ''

  # A DLL that changed after the app hashed it is not installed.
  $SourceDllSha256 = '0' * 64
  Reset-Check
  $threw = $false
  try { Install-WithoutDevices @() } catch { $threw = $true }
  Check 'all dropped: a changed DLL is refused' ($threw -and -not (Test-Path $InstalledDll) -and -not (Test-Path "$InstalledDll.new") -and $calls.Count -eq 0) "got '$($calls -join '|')'"
}
finally { Remove-Item -Recurse -Force $appDir }

# Only with no device left: with some left the run installs on them as before.
$main = $parsed[($scripts | Where-Object { $_ -like '*apo-install.ps1' })]
$call = $main.Find({ param($n) $n -is [System.Management.Automation.Language.CommandAst] -and $n.GetCommandName() -eq 'Install-WithoutDevices' }, $true)
$guard = $call
while ($guard -and $guard -isnot [System.Management.Automation.Language.IfStatementAst]) { $guard = $guard.Parent }
Check 'Install-WithoutDevices runs only when no job is left' ($guard -and $guard.Clauses[0].Item1.Extent.Text -eq '-not $jobs' -and $guard.Clauses[0].Item2.Extent.Text -match 'return') "guard '$($guard.Clauses[0].Item1.Extent.Text)'"

# --- The trap says why the run failed also without -FromApp (a silent run has only the log) -----
foreach ($script in $parsed.Keys | Where-Object { $_ -notlike '*apo-common.ps1' }) {
  $trap = $parsed[$script].Find({ param($n) $n -is [System.Management.Automation.Language.TrapStatementAst] }, $false)
  $say = if ($trap) {
    $trap.Body.Find({
        param($n) $n -is [System.Management.Automation.Language.CommandAst] -and $n.Extent.Text -match '^Say "`n(Install|Uninstall) failed: \$_'
      }, $true)
  }
  $inIf = $false
  for ($n = $say; $n -and $n -ne $trap; $n = $n.Parent) {
    if ($n -is [System.Management.Automation.Language.IfStatementAst]) { $inIf = $true }
  }
  Check "trap says the failure: $(Split-Path -Leaf $script)" ([bool]$say -and -not $inIf) 'missing, or only under a condition'
}

if ($failed) { throw "$failed of $total checks failed." }
Write-Host "apo-install: $total checks passed."
