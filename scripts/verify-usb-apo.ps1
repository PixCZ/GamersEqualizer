<#
.SYNOPSIS
  Safe, non-destructive APO diagnostic and verification script for USB audio endpoints.

.DESCRIPTION
  Snapshots FxProperties, verifies WASAPI formats and EventLog notes, tests candidate APO
  slots / processing modes / PKEY_FX_Association / legacy shadowing combinations one endpoint
  at a time, aborts immediately on open failures or audiodg.exe crashes, and restores exact
  original FxProperties values via .NET RegistryKey (SetValue + QueryValues, avoiding the
  reg.exe import ACL failure on FxProperties subkeys).

  Can run read-only with -InspectOnly or -BaselineOnly without Administrator privileges.
  Full mutating tests require an Administrator PowerShell window and restart Audiosrv.
#>
param(
  [string]$DeviceFilter = '',
  [string]$RestoreFrom = '',
  [string]$LogDir = (Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'GamersEqualizer\usb-apo-logs'),
  [switch]$InspectOnly,
  [switch]$BaselineOnly
)
$ErrorActionPreference = 'Stop'

$AppDir   = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer'
$ProbeExe = Join-Path $AppDir 'gamersequalizer.exe'

$FxGuid      = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$ModesGuid   = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$DefaultMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'
$NullGuid    = '{00000000-0000-0000-0000-000000000000}'
$MicClsid    = '{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}'
$OutClsid    = '{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}'
$StreamClsid = '{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}'
$GeClsids    = @($MicClsid, $OutClsid, $StreamClsid)

$AudioStopped = $false
$Held = New-Object System.Collections.Generic.List[string]
$script:ResultLog = $null

function LogLine([string]$text) {
  if ($script:ResultLog) {
    try { [System.IO.File]::AppendAllText($script:ResultLog, "$text`r`n", [System.Text.Encoding]::UTF8) } catch { }
  }
}

function Say([string]$text, [string]$color = '') {
  LogLine $text
  if ($script:AudioStopped) { $Held.Add($text) }
  elseif ($color) { Write-Host $text -ForegroundColor $color }
  else { Write-Host $text }
}

function Stop-Audio {
  $script:AudioStopped = $true
  Stop-Service Audiosrv -Force -WarningAction SilentlyContinue
}

function Start-Audio {
  Start-Service Audiosrv -WarningAction SilentlyContinue
  $script:AudioStopped = $false
  foreach ($line in $Held) { Write-Host $line }
  $Held.Clear()
}

function Get-FxSnapshot([string]$flow, [string]$id) {
  $path = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $k = Get-Item $path -ErrorAction SilentlyContinue
  if (-not $k) { return $null }
  try {
    $items = New-Object System.Collections.Generic.List[object]
    foreach ($vn in ($k.GetValueNames() | Sort-Object)) {
      $val = $k.GetValue($vn, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      $kind = $k.GetValueKind($vn)
      $items.Add([pscustomobject]@{ Name = $vn; Kind = $kind; Value = $val })
    }
    return [pscustomobject]@{ HasKey = $true; Items = $items.ToArray() }
  } finally {
    $k.Dispose()
  }
}

function Set-FxMulti([string]$flow, [string]$id, [string]$name, [string[]]$value) {
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    [string[]]$clean = @($value | Where-Object { $_ -and $_.Trim() -ne '' })
    if ($clean.Count -eq 0) { $key.DeleteValue($name, $false) }
    else { $key.SetValue($name, $clean, [Microsoft.Win32.RegistryValueKind]::MultiString) }
  } finally { $key.Dispose() }
}

function Set-FxString([string]$flow, [string]$id, [string]$name, [string]$value) {
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    if ([string]::IsNullOrWhiteSpace($value)) { $key.DeleteValue($name, $false) }
    else { $key.SetValue($name, $value, [Microsoft.Win32.RegistryValueKind]::String) }
  } finally { $key.Dispose() }
}

function Get-FxNames([string]$flow, [string]$id) {
  $path = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $k = Get-Item $path -ErrorAction SilentlyContinue
  if (-not $k) { return @() }
  try { return @($k.GetValueNames()) } finally { $k.Dispose() }
}

function Export-FxJson([string]$flow, [string]$id, [string]$jsonFile) {
  $snap = Get-FxSnapshot $flow $id
  $records = New-Object System.Collections.Generic.List[object]
  foreach ($item in $snap.Items) {
    $kindStr = [string]$item.Kind
    $encoded = switch ($kindStr) {
      'Binary'      { [Convert]::ToBase64String([byte[]]$item.Value) }
      'MultiString' { [string[]]@($item.Value | ForEach-Object { [string]$_ }) }
      default       { $item.Value }
    }
    $records.Add([pscustomobject]@{
      Name  = [string]$item.Name
      Kind  = $kindStr
      Value = $encoded
    })
  }
  ConvertTo-Json -InputObject @($records.ToArray()) -Depth 4 | Set-Content $jsonFile -Encoding UTF8
}

# Restores direct values only via SetValue + QueryValues; reg.exe import fails on FxProperties
# subkeys (Volatile/Default/User) because BUILTIN\Administrators lacks CreateSubKey permission.
function Restore-Exact([string]$flow, [string]$id, [string]$jsonFile) {
  $raw = Get-Content $jsonFile -Raw -Encoding UTF8 | ConvertFrom-Json
  $origRecords = @($raw | ForEach-Object { $_ })
  $origNames = @($origRecords | ForEach-Object { [string]$_.Name })
  $subKey = "SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow\$id\FxProperties"
  $curNames = Get-FxNames $flow $id
  $rights = [System.Security.AccessControl.RegistryRights]'SetValue, QueryValues'
  $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
  if ($null -eq $key) { throw "Key not found: HKLM\$subKey" }
  try {
    foreach ($n in $curNames) {
      if ($origNames -notcontains $n) { $key.DeleteValue($n, $false) }
    }
    foreach ($rec in $origRecords) {
      $kind = [Microsoft.Win32.RegistryValueKind][Enum]::Parse([Microsoft.Win32.RegistryValueKind], [string]$rec.Kind)
      $valName = [string]$rec.Name
      if ($kind -eq [Microsoft.Win32.RegistryValueKind]::Binary) {
        [byte[]]$binVal = [Convert]::FromBase64String([string]$rec.Value)
        $key.SetValue($valName, $binVal, $kind)
      } elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::MultiString) {
        [string[]]$multiVal = @($rec.Value | ForEach-Object { [string]$_ })
        $key.SetValue($valName, $multiVal, $kind)
      } elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::DWord) {
        [int]$dwVal = [int]$rec.Value
        $key.SetValue($valName, $dwVal, $kind)
      } elseif ($kind -eq [Microsoft.Win32.RegistryValueKind]::QWord) {
        [long]$qwVal = [long]$rec.Value
        $key.SetValue($valName, $qwVal, $kind)
      } else {
        [string]$strVal = [string]$rec.Value
        $key.SetValue($valName, $strVal, $kind)
      }
    }
  } finally {
    $key.Dispose()
  }
}

function Restore-FromDirectory([string]$dir) {
  $manifestPath = Join-Path $dir 'manifest.json'
  if (-not (Test-Path $manifestPath)) { throw "manifest.json not found in $dir" }
  $parsed = Get-Content $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
  $entries = @($parsed | ForEach-Object { $_ })
  Stop-Audio
  try {
    foreach ($e in $entries) {
      if ($e.HadFxKey) {
        Restore-Exact ([string]$e.Flow) ([string]$e.Id) ([string]$e.JsonFile)
      }
    }
  } finally {
    Start-Audio
  }
  Say "Restored exact original FxProperties from $dir" Green
}

function Assert-Admin {
  $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This experiment requires an Administrator PowerShell window (needed to backup/edit HKLM FxProperties and restart Audiosrv).'
  }
}

if ($RestoreFrom) {
  Assert-Admin
  Restore-FromDirectory $RestoreFrom
  Read-Host 'Press Enter to close'
  exit 0
}

function Decode-DeviceFormat([byte[]]$bytes) {
  if (-not $bytes -or $bytes.Length -lt 26) { return 'unknown' }
  $tag   = [BitConverter]::ToUInt16($bytes, 8)
  $ch    = [BitConverter]::ToUInt16($bytes, 10)
  $hz    = [BitConverter]::ToUInt32($bytes, 12)
  $bits  = [BitConverter]::ToUInt16($bytes, 22)
  $valid = if ($bytes.Length -ge 28 -and $tag -eq 0xFFFE) { [BitConverter]::ToUInt16($bytes, 26) } else { $bits }
  "tag=0x$($tag.ToString('X4')) $hz Hz, $ch ch, ${bits}bit (valid $valid)"
}

function Get-UsbTargets([string]$filter) {
  foreach ($flow in @('Capture', 'Render')) {
    foreach ($sub in @(Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow" -ErrorAction SilentlyContinue)) {
      if ($sub.GetValue('DeviceState') -ne 1) { continue }
      $id = $sub.PSChildName
      $propKey = Get-Item ($sub.PSPath + '\Properties') -ErrorAction SilentlyContinue
      if (-not $propKey) { continue }
      try {
        $name = [string]$propKey.GetValue('{b3f8fa53-0004-438e-9003-51a46e139bfc},6')
        $desc = [string]$propKey.GetValue('{a45c254e-df1c-4efd-8020-67d146a850e0},2')
        $enum = [string]$propKey.GetValue('{a45c254e-df1c-4efd-8020-67d146a850e0},24')
        $devId = [string]$propKey.GetValue('{b3f8fa53-0004-438e-9003-51a46e139bfc},2')
        $disableSysFx = $propKey.GetValue('{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5')
        $fmtBytes = $propKey.GetValue('{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0')
      } finally { $propKey.Dispose() }

      $isUsb = ($enum -eq 'USB') -or ($devId -match 'USB\\')
      if ($filter) {
        if ("$name $desc $id $enum $devId" -notmatch [regex]::Escape($filter)) { continue }
      } elseif (-not $isUsb) {
        continue
      }

      $fxSnap = Get-FxSnapshot $flow $id
      [pscustomobject]@{
        Flow = $flow; Id = $id; Name = $name; Desc = $desc; Enumerator = $enum; DevId = $devId
        DisableSysFx = $disableSysFx; Format = (Decode-DeviceFormat $fmtBytes)
        HasFxKey = ($null -ne $fxSnap); Fx = $(if ($fxSnap) { $fxSnap.Items } else { @() })
      }
    }
  }
}

function Append-Unique([string[]]$existing, [string]$clsid) {
  $res = New-Object System.Collections.Generic.List[string]
  foreach ($x in $existing) {
    if ($x -and $x.Trim() -ne '' -and ($res -notcontains $x)) { $res.Add($x) }
  }
  if (-not ($res | Where-Object { $_ -ieq $clsid })) { $res.Add($clsid) }
  return $res.ToArray()
}

function Strip-Ge([string[]]$existing) {
  @($existing | Where-Object { $item = $_; $item -and -not ($GeClsids | Where-Object { $_ -ieq $item }) })
}

function Get-OrigVal($fxItems, [string]$name) {
  $m = $fxItems | Where-Object { $_.Name -eq $name } | Select-Object -First 1
  if (-not $m) { return @() }
  if ($m.Value -is [string[]]) { return @($m.Value) }
  if ($m.Value) { return @([string]$m.Value) }
  return @()
}

# C# 5 compatible so it compiles under both Windows PowerShell 5.1 and PowerShell 7+.
$RenderProbeCs = @'
using System;
using System.Runtime.InteropServices;
using System.Threading;
public class UsbAudioProbe {
  [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDevEnum {}
  [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IMMDeviceEnumerator { int Unused(); int Unused2(); int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string id, out IMMDevice dev); }
  [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IMMDevice { int Activate(ref Guid iid, int clsCtx, IntPtr p, [MarshalAs(UnmanagedType.IUnknown)] out object iface); }
  [ComImport, Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IAudioClient {
    int Initialize(int mode, int flags, long bufDur, long per, IntPtr fmt, IntPtr sess);
    int GetBufferSize(out uint frames);
    int GetStreamLatency(out long lat);
    int GetCurrentPadding(out uint pad);
    int IsFormatSupported(int mode, IntPtr fmt, out IntPtr closest);
    int GetMixFormat(out IntPtr fmt);
    int GetDevicePeriod(out long def, out long min);
    int Start();
    int Stop();
    int Reset();
    int SetEventHandle(IntPtr h);
    int GetService(ref Guid iid, [MarshalAs(UnmanagedType.IUnknown)] out object svc);
  }
  [ComImport, Guid("F294ACFC-3146-4483-A7BF-ADDCA7C260E2"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IAudioRenderClient {
    int GetBuffer(uint frames, out IntPtr data);
    int ReleaseBuffer(uint frames, uint flags);
  }
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern IntPtr OpenFileMappingW(uint acc, bool inh, string name);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr h);
  public static string CheckShm() {
    IntPtr h = OpenFileMappingW(4, false, @"Global\GamersEqualizer_Apo_v19");
    int err = Marshal.GetLastWin32Error();
    if (h != IntPtr.Zero) { CloseHandle(h); return "OPEN"; }
    if (err == 5) return "EXISTS(AccessDenied)";
    return "MISSING(err=" + err + ")";
  }
  public static string GetMixFormatDesc(string id) {
    IMMDeviceEnumerator en = (IMMDeviceEnumerator)(new MMDevEnum());
    IMMDevice dev;
    Marshal.ThrowExceptionForHR(en.GetDevice(id, out dev));
    Guid iidClient = new Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2");
    object obj;
    Marshal.ThrowExceptionForHR(dev.Activate(ref iidClient, 23, IntPtr.Zero, out obj));
    IAudioClient client = (IAudioClient)obj;
    IntPtr fmt;
    Marshal.ThrowExceptionForHR(client.GetMixFormat(out fmt));
    try {
      short tag = Marshal.ReadInt16(fmt, 0);
      short ch = Marshal.ReadInt16(fmt, 2);
      int hz = Marshal.ReadInt32(fmt, 4);
      short bits = Marshal.ReadInt16(fmt, 14);
      return hz + "Hz " + ch + "ch " + bits + "bit tag=0x" + tag.ToString("X4");
    } finally {
      Marshal.FreeCoTaskMem(fmt);
    }
  }
  public static string RunRender(string id) {
    IMMDeviceEnumerator en = (IMMDeviceEnumerator)(new MMDevEnum());
    IMMDevice dev;
    Marshal.ThrowExceptionForHR(en.GetDevice(id, out dev));
    Guid iidClient = new Guid("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2");
    object obj;
    Marshal.ThrowExceptionForHR(dev.Activate(ref iidClient, 23, IntPtr.Zero, out obj));
    IAudioClient client = (IAudioClient)obj;
    IntPtr fmt;
    Marshal.ThrowExceptionForHR(client.GetMixFormat(out fmt));
    short align = Marshal.ReadInt16(fmt, 12);
    Marshal.ThrowExceptionForHR(client.Initialize(0, 0, 1000000, 0, fmt, IntPtr.Zero));
    Marshal.FreeCoTaskMem(fmt);
    uint bufSize;
    Marshal.ThrowExceptionForHR(client.GetBufferSize(out bufSize));
    Guid iidRender = new Guid("F294ACFC-3146-4483-A7BF-ADDCA7C260E2");
    object rObj;
    Marshal.ThrowExceptionForHR(client.GetService(ref iidRender, out rObj));
    IAudioRenderClient render = (IAudioRenderClient)rObj;
    Marshal.ThrowExceptionForHR(client.Start());
    string shmState = "MISSING";
    int end = Environment.TickCount + 1600;
    while (Environment.TickCount < end) {
      uint pad;
      Marshal.ThrowExceptionForHR(client.GetCurrentPadding(out pad));
      uint avail = bufSize - pad;
      if (avail > 0) {
        IntPtr data;
        Marshal.ThrowExceptionForHR(render.GetBuffer(avail, out data));
        byte[] zeros = new byte[avail * align];
        Marshal.Copy(zeros, 0, data, (int)(avail * align));
        Marshal.ThrowExceptionForHR(render.ReleaseBuffer(avail, 0));
      }
      string s = CheckShm();
      if (s == "OPEN" || s.StartsWith("EXISTS")) shmState = s;
      else if (shmState == "MISSING") shmState = s;
      Thread.Sleep(25);
    }
    client.Stop();
    return shmState;
  }
}
'@
Add-Type -TypeDefinition $RenderProbeCs

function Get-NotesSince([datetime]$since) {
  $ge = @()
  $cr = @()
  $readable = $true
  try {
    $ge = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'GamersEqualizer'; StartTime = $since } -ErrorAction Stop |
      Select-Object -First 120 |
      ForEach-Object { "$($_.TimeCreated.ToString('HH:mm:ss.fff')) $($_.Properties[0].Value)" })
    if ($ge.Count -gt 1) { [array]::Reverse($ge) }
  } catch [Exception] {
    if ($_.Exception.Message -notmatch 'No events were found|Nebyly nalezeny') { $readable = $false }
  }
  try {
    $cr = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'Application Error'; Id = 1000; StartTime = $since } -ErrorAction Stop |
      Select-Object -First 20 |
      Where-Object { $_.Message -match 'audiodg\.exe' } |
      ForEach-Object { "CRASH $($_.TimeCreated.ToString('HH:mm:ss.fff')) $($_.Message -replace '\s+',' ')" })
  } catch [Exception] {
    if ($_.Exception.Message -notmatch 'No events were found|Nebyly nalezeny') { $readable = $false }
  }
  [pscustomobject]@{ Notes = $ge; Crashes = $cr; Readable = $readable }
}

function Invoke-MicProbe([string]$capId, [string]$label, [datetime]$sinceRestart, [bool]$waitAfterRestart) {
  if ($waitAfterRestart) { Start-Sleep -Milliseconds 1800 }
  $preShm = [UsbAudioProbe]::CheckShm()
  $preEv = Get-NotesSince $sinceRestart
  foreach ($n in $preEv.Notes) { Say "    [pre-probe] $n" }

  $probeStart = Get-Date
  $p = $null
  $shm = 'MISSING'
  for ($try = 1; $try -le 3; $try++) {
    $shm = 'MISSING'
    $p = Start-Process -FilePath $ProbeExe -ArgumentList '--probe-mic', $capId -NoNewWindow -PassThru
    $null = $p.Handle
    $deadline = (Get-Date).AddSeconds(20)
    while (-not $p.HasExited) {
      if ((Get-Date) -gt $deadline) { $p.Kill(); throw "$label`: --probe-mic hung" }
      $s = [UsbAudioProbe]::CheckShm()
      if ($s -eq 'OPEN' -or $s.StartsWith('EXISTS')) { $shm = $s }
      elseif ($shm -eq 'MISSING') { $shm = $s }
      Start-Sleep -Milliseconds 100
    }
    $sAfter = [UsbAudioProbe]::CheckShm()
    if ($sAfter -eq 'OPEN' -or $sAfter.StartsWith('EXISTS')) { $shm = $sAfter }
    elseif ($shm -eq 'MISSING') { $shm = $sAfter }
    if ($p.ExitCode -eq 0 -or $try -eq 3) { break }
    Start-Sleep -Seconds 1
  }
  $ev = Get-NotesSince $probeStart
  if (-not $ev.Readable -and -not $BaselineOnly) { throw 'Cannot read Windows Application EventLog during experiment.' }
  Say "  [$label] exit=$($p.ExitCode), preShm=$preShm, shm=$shm, evLog=$($ev.Readable), crashes=$($ev.Crashes.Count + $preEv.Crashes.Count)" Yellow
  foreach ($c in @($preEv.Crashes + $ev.Crashes)) { Say "    $c" Red }
  foreach ($n in $ev.Notes) { Say "    $n" }
  $allNotes = @($preEv.Notes + $ev.Notes)
  $startedProbe = [bool]($ev.Notes | Where-Object { $_ -match 'GamersEqualizer Mic Chain(?: \[[^\]]+\])?: stream .* started' })
  $startedAny   = [bool]($allNotes | Where-Object { $_ -match 'GamersEqualizer Mic Chain(?: \[[^\]]+\])?: stream .* started' })
  [pscustomobject]@{
    Ok           = ($p.ExitCode -eq 0 -and ($ev.Crashes.Count + $preEv.Crashes.Count) -eq 0)
    Started      = $startedProbe
    PreStarted   = ($startedAny -and -not $startedProbe)
    AnyStarted   = $startedAny
    Shm          = $shm
    Label        = $label
  }
}

function Invoke-RenderProbe([string]$renId, [string]$label, [datetime]$sinceRestart, [bool]$waitAfterRestart) {
  if ($waitAfterRestart) { Start-Sleep -Milliseconds 1800 }
  $preShm = [UsbAudioProbe]::CheckShm()
  $preEv = Get-NotesSince $sinceRestart
  foreach ($n in $preEv.Notes) { Say "    [pre-probe] $n" }

  $probeStart = Get-Date
  $shm = 'not-run'
  $mix = 'unknown'
  $err = $null
  for ($try = 1; $try -le 3; $try++) {
    $err = $null
    try {
      $fullId = "{0.0.0.00000000}.$renId"
      $mix = [UsbAudioProbe]::GetMixFormatDesc($fullId)
      $shm = [UsbAudioProbe]::RunRender($fullId)
      break
    } catch {
      $err = $_.Exception.Message
      if ($try -lt 3) { Start-Sleep -Seconds 1 }
    }
  }
  $ev = Get-NotesSince $probeStart
  if (-not $ev.Readable -and -not $BaselineOnly) { throw 'Cannot read Windows Application EventLog during experiment.' }
  Say "  [$label] mix=[$mix], err='$err', preShm=$preShm, shm=$shm, evLog=$($ev.Readable), crashes=$($ev.Crashes.Count + $preEv.Crashes.Count)" Yellow
  foreach ($c in @($preEv.Crashes + $ev.Crashes)) { Say "    $c" Red }
  foreach ($n in $ev.Notes) { Say "    $n" }
  $allNotes  = @($preEv.Notes + $ev.Notes)
  $outProbe  = [bool]($ev.Notes | Where-Object { $_ -match 'GamersEqualizer Output EQ(?: \[[^\]]+\])?: stream .* started' })
  $strProbe  = [bool]($ev.Notes | Where-Object { $_ -match 'GamersEqualizer Stream EQ(?: \[[^\]]+\])?: stream .* started' })
  $outPre    = [bool]($preEv.Notes | Where-Object { $_ -match 'GamersEqualizer Output EQ(?: \[[^\]]+\])?: stream .* started' })
  $strPre    = [bool]($preEv.Notes | Where-Object { $_ -match 'GamersEqualizer Stream EQ(?: \[[^\]]+\])?: stream .* started' })
  $outAny    = [bool]($allNotes | Where-Object { $_ -match 'GamersEqualizer Output EQ(?: \[[^\]]+\])?: stream .* started' })
  $strAny    = [bool]($allNotes | Where-Object { $_ -match 'GamersEqualizer Stream EQ(?: \[[^\]]+\])?: stream .* started' })
  [pscustomobject]@{
    Ok               = ($null -eq $err -and ($ev.Crashes.Count + $preEv.Crashes.Count) -eq 0)
    OutStarted       = $outProbe
    StreamStarted    = $strProbe
    OutAnyStarted    = $outAny
    StreamAnyStarted = $strAny
    PreOnly          = (($outPre -or $strPre) -and -not ($outProbe -and $strProbe))
    Shm              = $shm
    Label            = $label
  }
}

# --- Verify prerequisites ---
if (-not $InspectOnly) {
  if (-not (Test-Path $ProbeExe)) { throw "Missing $ProbeExe" }
  foreach ($c in $GeClsids) {
    if (-not (Test-Path "Registry::HKEY_CLASSES_ROOT\CLSID\$c\InprocServer32")) {
      throw "COM registration missing for $c. Run normal app install first."
    }
  }
}

$targets = @(Get-UsbTargets $DeviceFilter)
if ($targets.Count -eq 0) {
  Write-Host 'No active USB audio endpoints matched.' -ForegroundColor Red
  exit 1
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$modeTag = if ($BaselineOnly) { 'baseline' } elseif ($InspectOnly) { 'inspect' } else { 'experiment' }
$firstSlug = if ($targets.Count -gt 1) { "$($targets.Count)usb" } else { ($targets[0].Name -replace '[^A-Za-z0-9_-]', '_') }
if ([string]::IsNullOrWhiteSpace($firstSlug)) { $firstSlug = 'usb' }
New-Item -ItemType Directory -Force $LogDir | Out-Null
$script:ResultLog = Join-Path $LogDir "usb-apo-$stamp-$modeTag-$firstSlug.txt"
$BackupDir = Join-Path $AppDir "apo-backup\usb-exp-$stamp"

[System.IO.File]::WriteAllText($script:ResultLog, "USB APO $modeTag $stamp`r`nFilter='$DeviceFilter' InspectOnly=$InspectOnly BaselineOnly=$BaselineOnly`r`n", [System.Text.Encoding]::UTF8)

Say "Detected $($targets.Count) active target endpoint(s):" Cyan
foreach ($t in $targets) {
  $epFullId = "{0.0.$(if ($t.Flow -eq 'Capture') { '1' } else { '0' }).00000000}.$($t.Id)"
  $liveMix = try { [UsbAudioProbe]::GetMixFormatDesc($epFullId) } catch { "err: $($_.Exception.Message)" }
  Say "  $($t.Flow) $($t.Id) | $($t.Name) ($($t.Desc)) | Enum=$($t.Enumerator) | DevId=$($t.DevId)"
  Say "    DisableSysFx=$($t.DisableSysFx) | RegFmt=$($t.Format) | LiveMix=[$liveMix]"
  if ($t.HasFxKey) {
    if ($t.Fx.Count -eq 0) { Say '    Fx: (empty FxProperties key)' Yellow }
    foreach ($item in $t.Fx) {
      $disp = if ($item.Value -is [byte[]]) { ($item.Value -join ' ') } elseif ($item.Value -is [string[]]) { ($item.Value -join '; ') } else { [string]$item.Value }
      Say "    Fx[$($item.Kind)] $($item.Name) = $disp"
    }
  } else {
    Say '    Fx: (missing FxProperties key!)' Red
  }
}
Say "Log file: $script:ResultLog" Green

if ($InspectOnly) { exit 0 }

if ($BaselineOnly) {
  Say "`n--- Passive Baseline Probes (no registry edits, no Audiosrv restart) ---" Cyan
  foreach ($cap in ($targets | Where-Object Flow -eq 'Capture')) {
    $t = Get-Date
    Say "`n=== Capture $($cap.Id) ($($cap.Name)) ==="
    $null = Invoke-MicProbe $cap.Id 'C0: passive baseline' $t $false
  }
  foreach ($ren in ($targets | Where-Object Flow -eq 'Render')) {
    $t = Get-Date
    Say "`n=== Render $($ren.Id) ($($ren.Name)) ==="
    $null = Invoke-RenderProbe $ren.Id 'R0: passive baseline' $t $false
  }
  Say "`nBaseline-only finished. Log saved to $script:ResultLog" Green
  exit 0
}

Assert-Admin

$missingFx = @($targets | Where-Object { -not $_.HasFxKey })
if ($missingFx.Count -gt 0) {
  Say 'One or more endpoints have no FxProperties key; aborting safely without modifying registry.' Yellow
  exit 0
}

Write-Host ''
Write-Host 'IMPORTANT: Do NOT click inside this window while audio stops/restarts.' -ForegroundColor Yellow
$null = Read-Host "Press Enter to test $($targets.Count) USB endpoint(s) with automatic rollback (or Ctrl+C to cancel)"

# --- Backup exact initial state ---
New-Item -ItemType Directory -Force $BackupDir | Out-Null
$manifest = New-Object System.Collections.Generic.List[object]
$backupMap = @{}
foreach ($t in $targets) {
  $jsonFile = Join-Path $BackupDir "$($t.Flow)-$($t.Id).json"
  $regFile  = Join-Path $BackupDir "$($t.Flow)-$($t.Id).reg"
  Export-FxJson $t.Flow $t.Id $jsonFile
  & reg.exe export "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$($t.Flow)\$($t.Id)\FxProperties" $regFile /y | Out-Null
  $entry = [pscustomobject]@{
    Flow = $t.Flow; Id = $t.Id; HadFxKey = $true; JsonFile = $jsonFile; RegFile = $regFile
  }
  $manifest.Add($entry)
  $backupMap["$($t.Flow):$($t.Id)"] = $entry
}
ConvertTo-Json -InputObject @($manifest.ToArray()) -Depth 4 | Set-Content (Join-Path $BackupDir 'manifest.json') -Encoding UTF8
Say "Backups saved to $BackupDir" Green

function Restore-All {
  Restore-FromDirectory $BackupDir
}

function Restore-One([string]$flow, [string]$id) {
  $e = $backupMap["$($flow):$($id)"]
  if (-not $e) { return }
  Stop-Audio
  try {
    Restore-Exact $e.Flow $e.Id $e.JsonFile
  } finally {
    Start-Audio
  }
  Say "  Restored $flow $id to original baseline." Cyan
}

$summary = New-Object System.Collections.Generic.List[string]

try {
  foreach ($cap in ($targets | Where-Object Flow -eq 'Capture')) {
    $id = $cap.Id
    $origFx = $cap.Fx
    $orig0  = (Get-OrigVal $origFx "$FxGuid,0") | Select-Object -First 1
    $orig5  = (Get-OrigVal $origFx "$FxGuid,5") | Select-Object -First 1
    $orig6  = Strip-Ge (Get-OrigVal $origFx "$FxGuid,6")
    $orig7  = Strip-Ge (Get-OrigVal $origFx "$FxGuid,7")
    $orig13 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,13")
    $orig14 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,14")
    $orig15 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,15")
    $origM5 = Get-OrigVal $origFx "$ModesGuid,5"
    $origM6 = Get-OrigVal $origFx "$ModesGuid,6"
    $origM7 = Get-OrigVal $origFx "$ModesGuid,7"
    $hadGe  = (@(Get-OrigVal $origFx "$FxGuid,15"; Get-OrigVal $origFx "$FxGuid,14"; Get-OrigVal $origFx "$FxGuid,13"; Get-OrigVal $origFx "$FxGuid,7"; Get-OrigVal $origFx "$FxGuid,6") |
               Where-Object { $_ -ieq $MicClsid }).Count -gt 0
    $modified = $false

    Say "`n=== Capture $id ($($cap.Name) / $($cap.Desc) | $($cap.Format)) ===" Cyan

    # [C0] Baseline
    $t = Get-Date
    Stop-Audio; Start-Audio
    $r = Invoke-MicProbe $id 'C0: baseline' $t $true
    if (-not $r.Ok) { throw "C0 baseline mic probe failed for $id!" }

    # [C0a] Stock apo-install (only if GE wasn't yet present)
    if (-not $r.AnyStarted -and -not $hadGe) {
      Say "`n[C0a] Stock apo-install (,15 += Mic Chain, no modes, no ,0)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try { Set-FxMulti 'Capture' $id "$FxGuid,15" (Append-Unique $orig15 $MicClsid) } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C0a: stock ,15' $t $true
      if (-not $r.Ok) { throw "C0a broke mic open or crashed audiodg!" }
    }

    # [C1] Composite EFX + EFX modes (,7), keeping original ,0
    if (-not $r.AnyStarted) {
      Say "`n[C1] Composite EFX (,15) + EFX modes (,7 = Default), keeping original ,0..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,15" (Append-Unique $orig15 $MicClsid)
        Set-FxMulti 'Capture' $id "$FxGuid,14" $orig14
        Set-FxMulti 'Capture' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C1: EFX + modes' $t $true
      if (-not $r.Ok) { throw "C1 broke mic open or crashed audiodg!" }
    }

    # [C2] Composite MFX + MFX modes (,6), keeping original ,0
    if (-not $r.AnyStarted) {
      Say "`n[C2] Composite MFX (,14) + MFX modes (,6 = Default), keeping original ,0..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,14" (Append-Unique $orig14 $MicClsid)
        Set-FxMulti 'Capture' $id "$FxGuid,15" $orig15
        Set-FxMulti 'Capture' $id "$ModesGuid,7" $origM7
        Set-FxMulti 'Capture' $id "$ModesGuid,6" (Append-Unique $origM6 $DefaultMode)
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C2: MFX + modes' $t $true
      if (-not $r.Ok) { throw "C2 broke mic open or crashed audiodg!" }
    }

    # [C1b] Composite EFX + PKEY_FX_Association (,0) WITHOUT adding modes,7
    if (-not $r.AnyStarted -and -not $orig0) {
      Say "`n[C1b] Composite EFX (,15) + PKEY_FX_Association (,0) WITHOUT modes,7..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,15" (Append-Unique $orig15 $MicClsid)
        Set-FxMulti 'Capture' $id "$FxGuid,14" $orig14
        Set-FxMulti 'Capture' $id "$ModesGuid,6" $origM6
        Set-FxMulti 'Capture' $id "$ModesGuid,7" $origM7
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C1b: EFX + ,0 (no modes)' $t $true
      if (-not $r.Ok) { throw "C1b broke mic open or crashed audiodg!" }
    }

    # [C3] Composite EFX + EFX modes (,7) + PKEY_FX_Association (,0)
    if (-not $r.AnyStarted) {
      Say "`n[C3] Composite EFX (,15) + EFX modes (,7) + PKEY_FX_Association (,0)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,15" (Append-Unique $orig15 $MicClsid)
        Set-FxMulti 'Capture' $id "$FxGuid,14" $orig14
        Set-FxMulti 'Capture' $id "$ModesGuid,6" $origM6
        Set-FxMulti 'Capture' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C3: EFX + modes + ,0' $t $true
      if (-not $r.Ok) { throw "C3 broke mic open or crashed audiodg!" }
    }

    # [C4] Composite MFX + MFX modes (,6) + PKEY_FX_Association (,0)
    if (-not $r.AnyStarted) {
      Say "`n[C4] Composite MFX (,14) + MFX modes (,6) + PKEY_FX_Association (,0)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,14" (Append-Unique $orig14 $MicClsid)
        Set-FxMulti 'Capture' $id "$FxGuid,15" $orig15
        Set-FxMulti 'Capture' $id "$ModesGuid,7" $origM7
        Set-FxMulti 'Capture' $id "$ModesGuid,6" (Append-Unique $origM6 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C4: MFX + modes + ,0' $t $true
      if (-not $r.Ok) { throw "C4 broke mic open or crashed audiodg!" }
    }

    # [C5] Legacy single-string EFX (,7) + modes + ,0
    if (-not $r.AnyStarted -and $orig7.Count -eq 0) {
      Say "`n[C5] Legacy REG_SZ EFX (,7 = Mic Chain) + modes (,7) + ,0 (no ,14/,15)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,14" @()
        Set-FxMulti 'Capture' $id "$FxGuid,15" @()
        Set-FxMulti 'Capture' $id "$ModesGuid,6" $origM6
        Set-FxMulti 'Capture' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
        Set-FxString 'Capture' $id "$FxGuid,7" $MicClsid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C5: legacy ,7 + modes + ,0' $t $true
      if (-not $r.Ok) { throw "C5 broke mic open or crashed audiodg!" }
    }

    # [C6] Legacy single-string MFX (,6) + modes + ,0
    if (-not $r.AnyStarted -and $orig6.Count -eq 0) {
      Say "`n[C6] Legacy REG_SZ MFX (,6 = Mic Chain) + modes (,6) + ,0 (no ,14/,15)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Capture' $id "$FxGuid,14" @()
        Set-FxMulti 'Capture' $id "$FxGuid,15" @()
        Set-FxString 'Capture' $id "$FxGuid,7" ($orig7 | Select-Object -First 1)
        Set-FxMulti 'Capture' $id "$ModesGuid,7" $origM7
        Set-FxMulti 'Capture' $id "$ModesGuid,6" (Append-Unique $origM6 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
        Set-FxString 'Capture' $id "$FxGuid,6" $MicClsid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C6: legacy ,6 + modes + ,0' $t $true
      if (-not $r.Ok) { throw "C6 broke mic open or crashed audiodg!" }
    }

    # [C7] Composite SFX (,13) + modes (,5) + ,0
    if (-not $r.AnyStarted) {
      Say "`n[C7] Composite SFX (,13 = Mic Chain) + SFX modes (,5) + ,0..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxString 'Capture' $id "$FxGuid,6" ($orig6 | Select-Object -First 1)
        Set-FxString 'Capture' $id "$FxGuid,7" ($orig7 | Select-Object -First 1)
        Set-FxMulti 'Capture' $id "$FxGuid,14" $orig14
        Set-FxMulti 'Capture' $id "$FxGuid,15" $orig15
        Set-FxMulti 'Capture' $id "$FxGuid,13" (Append-Unique $orig13 $MicClsid)
        Set-FxMulti 'Capture' $id "$ModesGuid,6" $origM6
        Set-FxMulti 'Capture' $id "$ModesGuid,7" $origM7
        Set-FxMulti 'Capture' $id "$ModesGuid,5" (Append-Unique $origM5 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C7: SFX + modes + ,0' $t $true
      if (-not $r.Ok) { throw "C7 broke mic open or crashed audiodg!" }
    }

    # [C8] Last-resort check if C0..C7 failed and VocaEffectPack is present
    $origPack = (Get-OrigVal $origFx '{9e6136e0-57ab-4949-b57a-3627be142855},100') | Select-Object -First 1
    if (-not $r.AnyStarted -and $origPack) {
      Say "`n[C8] Testing without EffectPack ({9e6136e0-...},100) + EFX (,15) + modes,7 + ,0..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxString 'Capture' $id '{9e6136e0-57ab-4949-b57a-3627be142855},100' ''
        Set-FxMulti 'Capture' $id "$FxGuid,13" $orig13
        Set-FxMulti 'Capture' $id "$FxGuid,14" $orig14
        Set-FxMulti 'Capture' $id "$FxGuid,15" (Append-Unique $orig15 $MicClsid)
        Set-FxMulti 'Capture' $id "$ModesGuid,5" $origM5
        Set-FxMulti 'Capture' $id "$ModesGuid,6" $origM6
        Set-FxMulti 'Capture' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
        Set-FxString 'Capture' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $r = Invoke-MicProbe $id 'C8: EFX + modes + ,0 (no EffectPack)' $t $true
      if (-not $r.Ok) { throw "C8 broke mic open or crashed audiodg!" }
    }

    $msg = "Capture $($cap.Name) ($id): Started=$($r.AnyStarted) (preOnly=$($r.PreStarted)) at [$($r.Label)] shm=$($r.Shm)"
    Say $msg $(if ($r.AnyStarted) { 'Green' } else { 'Red' })
    $summary.Add($msg)
    if ($modified) { Restore-One 'Capture' $id }
  }

  foreach ($ren in ($targets | Where-Object Flow -eq 'Render')) {
    $id = $ren.Id
    $origFx = $ren.Fx
    $orig0  = (Get-OrigVal $origFx "$FxGuid,0") | Select-Object -First 1
    $orig5  = (Get-OrigVal $origFx "$FxGuid,5") | Select-Object -First 1
    $orig6  = (Get-OrigVal $origFx "$FxGuid,6") | Select-Object -First 1
    $orig7  = (Get-OrigVal $origFx "$FxGuid,7") | Select-Object -First 1
    $orig13 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,13")
    $orig14 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,14")
    $orig15 = Strip-Ge (Get-OrigVal $origFx "$FxGuid,15")
    $base13 = if ($orig13.Count -gt 0) { $orig13 } elseif ($orig5) { @($orig5) } else { @() }
    $base14 = if ($orig14.Count -gt 0) { $orig14 } elseif ($orig6) { @($orig6) } else { @() }
    $base15 = if ($orig15.Count -gt 0) { $orig15 } elseif ($orig7) { @($orig7) } else { @() }
    $origM5 = Get-OrigVal $origFx "$ModesGuid,5"
    $origM6 = Get-OrigVal $origFx "$ModesGuid,6"
    $origM7 = Get-OrigVal $origFx "$ModesGuid,7"
    $hadGe  = (@(Get-OrigVal $origFx "$FxGuid,13"; Get-OrigVal $origFx "$FxGuid,14"; Get-OrigVal $origFx "$FxGuid,15") |
               Where-Object { $_ -ieq $OutClsid -or $_ -ieq $StreamClsid }).Count -gt 0
    $modified = $false

    Say "`n=== Render $id ($($ren.Name) / $($ren.Desc) | $($ren.Format)) ===" Cyan

    # [R0] Baseline
    $t = Get-Date
    Stop-Audio; Start-Audio
    $rr = Invoke-RenderProbe $id 'R0: baseline' $t $true
    if (-not $rr.Ok) { throw "R0 baseline render probe failed for $id!" }

    # [R0a] Stock apo-install if untouched endpoint
    if ((-not $rr.OutAnyStarted -or -not $rr.StreamAnyStarted) -and -not $hadGe) {
      Say "`n[R0a] Stock apo-install (,13 += Stream EQ, ,15 += Output EQ, no modes/no ,0)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Render' $id "$FxGuid,13" (Append-Unique $base13 $StreamClsid)
        Set-FxMulti 'Render' $id "$FxGuid,15" (Append-Unique $base15 $OutClsid)
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R0a: stock ,13+,15' $t $true
      if (-not $rr.Ok) { throw "R0a broke render open or crashed audiodg!" }
    }

    # [R1] + EFX modes (,7) and SFX modes (,5 if absent), NO ,0 yet
    if (-not $rr.OutAnyStarted -or -not $rr.StreamAnyStarted) {
      Say "`n[R1] Composite ,13+,15 + EFX modes (,7) & SFX modes (,5), no ,0..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Render' $id "$FxGuid,13" (Append-Unique $base13 $StreamClsid)
        Set-FxMulti 'Render' $id "$FxGuid,15" (Append-Unique $base15 $OutClsid)
        Set-FxMulti 'Render' $id "$ModesGuid,5" (Append-Unique $origM5 $DefaultMode)
        Set-FxMulti 'Render' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R1: EFX+SFX + modes' $t $true
      if (-not $rr.Ok) { throw "R1 broke render open or crashed audiodg!" }
    }

    # [R1b] Composite ,13+,15 + PKEY_FX_Association (,0) WITHOUT adding modes,7
    if ((-not $rr.OutAnyStarted -or -not $rr.StreamAnyStarted) -and -not $orig0) {
      Say "`n[R1b] Composite ,13+,15 + PKEY_FX_Association (,0) WITHOUT modes,7..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Render' $id "$ModesGuid,7" $origM7
        Set-FxString 'Render' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R1b: EFX+SFX + ,0 (no modes,7)' $t $true
      if (-not $rr.Ok) { throw "R1b broke render open or crashed audiodg!" }
    }

    # [R2] + EFX modes (,7) + PKEY_FX_Association (,0)
    if (-not $rr.OutAnyStarted -or -not $rr.StreamAnyStarted) {
      Say "`n[R2] Composite ,13+,15 + modes (,5/,7) + PKEY_FX_Association (,0)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Render' $id "$ModesGuid,5" (Append-Unique $origM5 $DefaultMode)
        Set-FxMulti 'Render' $id "$ModesGuid,7" (Append-Unique $origM7 $DefaultMode)
        Set-FxString 'Render' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R2: EFX+SFX + modes + ,0' $t $true
      if (-not $rr.Ok) { throw "R2 broke render open or crashed audiodg!" }
    }

    # [R3] Clear legacy ,5 and ,6 (promoting ,6 to ,14 if needed so OEM MFX stays intact)
    if ((-not $rr.OutAnyStarted -or -not $rr.StreamAnyStarted) -and ($orig5 -or $orig6)) {
      Say "`n[R3] Clearing legacy ,5 and ,6 (keeping OEM CLSIDs in composite ,13/,14)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        if ($base14.Count -gt 0) { Set-FxMulti 'Render' $id "$FxGuid,14" $base14 }
        Set-FxString 'Render' $id "$FxGuid,5" ''
        Set-FxString 'Render' $id "$FxGuid,6" ''
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R3: cleared legacy ,5/,6' $t $true
      if (-not $rr.Ok) { throw "R3 broke render open or crashed audiodg!" }
    }

    # [R4] Output EQ in MFX (,14) instead of EFX (,15), keeping original ,5/,6
    if (-not $rr.OutAnyStarted) {
      Say "`n[R4] Moving Output EQ from EFX (,15) to MFX (,14), keeping original ,5/,6..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxString 'Render' $id "$FxGuid,5" $orig5
        Set-FxString 'Render' $id "$FxGuid,6" $orig6
        Set-FxMulti 'Render' $id "$FxGuid,14" (Append-Unique $base14 $OutClsid)
        Set-FxMulti 'Render' $id "$FxGuid,15" $orig15
        Set-FxMulti 'Render' $id "$ModesGuid,7" $origM7
        Set-FxMulti 'Render' $id "$ModesGuid,6" (Append-Unique $origM6 $DefaultMode)
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R4: Output EQ in MFX (,14)' $t $true
      if (-not $rr.Ok) { throw "R4 broke render open or crashed audiodg!" }
    }

    # [R4b] Output EQ in MFX (,14) WITH legacy ,5/,6 cleared
    if (-not $rr.OutAnyStarted -and ($orig5 -or $orig6)) {
      Say "`n[R4b] Output EQ in MFX (,14) with legacy ,5/,6 cleared..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxString 'Render' $id "$FxGuid,5" ''
        Set-FxString 'Render' $id "$FxGuid,6" ''
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R4b: Output EQ in MFX (no legacy ,5/,6)' $t $true
      if (-not $rr.Ok) { throw "R4b broke render open or crashed audiodg!" }
    }

    # [R5] Legacy REG_SZ fallback (,5 = Stream EQ, ,6 = Output EQ, clearing composite ,13/,14/,15)
    if (-not $rr.OutAnyStarted) {
      Say "`n[R5] Legacy REG_SZ fallback (,5 = Stream EQ, ,6 = Output EQ, clearing ,13/,14/,15)..."
      $modified = $true
      $t = Get-Date; Stop-Audio
      try {
        Set-FxMulti 'Render' $id "$FxGuid,13" @()
        Set-FxMulti 'Render' $id "$FxGuid,14" @()
        Set-FxMulti 'Render' $id "$FxGuid,15" @()
        Set-FxString 'Render' $id "$FxGuid,5" $StreamClsid
        Set-FxString 'Render' $id "$FxGuid,6" $OutClsid
        Set-FxMulti 'Render' $id "$ModesGuid,5" (Append-Unique $origM5 $DefaultMode)
        Set-FxMulti 'Render' $id "$ModesGuid,6" (Append-Unique $origM6 $DefaultMode)
        Set-FxString 'Render' $id "$FxGuid,0" $NullGuid
      } finally { Start-Audio }
      $rr = Invoke-RenderProbe $id 'R5: legacy ,5+,6 + modes + ,0' $t $true
      if (-not $rr.Ok) { throw "R5 broke render open or crashed audiodg!" }
    }

    $msg = "Render $($ren.Name) ($id): Stream=$($rr.StreamAnyStarted) Output=$($rr.OutAnyStarted) (preOnly=$($rr.PreOnly)) at [$($rr.Label)] shm=$($rr.Shm)"
    Say $msg $(if ($rr.StreamAnyStarted -and $rr.OutAnyStarted) { 'Green' } else { 'Yellow' })
    $summary.Add($msg)
    if ($modified) { Restore-One 'Render' $id }
  }

  Say "`n=== SUMMARY ===" Cyan
  foreach ($line in $summary) { Say "  $line" }
  Say "`nAll tested endpoints were restored to exact original FxProperties (0 crashes, 0 open failures)." Green
} catch {
  Say "`nERROR: $_" Red
  Say 'Rolling back ALL endpoints to exact original FxProperties...' Yellow
  try { Restore-All } catch { Start-Service Audiosrv -ErrorAction SilentlyContinue; Say "Rollback error: $_" Red }
  Read-Host 'Press Enter to close'
  exit 1
} finally {
  Start-Service Audiosrv -ErrorAction SilentlyContinue
}
Read-Host "Done. Log saved to $script:ResultLog. Press Enter to close"
