# Shared by apo-install.ps1 and apo-uninstall.ps1. The app puts this text in place of their
# `# @common` line before it writes and hashes them (win_audio/apo_install/elevated.rs), so the elevated
# run is still one checked text; run from disk, they dot-source this file.

# Known-folder lookup, not $env:ProgramFiles: the user can override that variable in
# HKCU\Environment and this elevated process inherits it (DLL for audiodg in a user folder).
$ProgramDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'GamersEqualizer'
$InstallDir = Join-Path $ProgramDir 'apo'
$InstalledExe = Join-Path $ProgramDir 'gamersequalizer.exe'
$BackupDir = Join-Path $ProgramDir 'apo-backup'
# winget finds the installed app by this key; apo-uninstall.ps1 removes it and the shortcut.
$UninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\GamersEqualizer'
$StartMenuLink = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'GamersEqualizer.lnk'

$FxGuid = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$NullGuid = '{00000000-0000-0000-0000-000000000000}'
# Fixed Microsoft Windows SDK PKEY_SFX/MFX/EFX_ProcessingModes_Supported_For_Streaming + DEFAULT mode.
$ModesGuid = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'
$DefaultProcessingMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'
$ModeValues = @{ SFX = "$ModesGuid,5"; MFX = "$ModesGuid,6"; EFX = "$ModesGuid,7" }

# An endpoint whose driver declares no effects (inbox "High Definition Audio Device", VMs) has no
# FxProperties, and Administrators may not create subkeys under an endpoint (SetValue + ReadKey
# only). The backup/restore privileges, which Administrators hold but must enable, create and
# delete the key without changing the owner or ACL of anything that exists; the new key inherits
# the endpoint's ACL. C# 5 (Windows PowerShell's compiler).
$RestoreKeySource = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace GamersEqualizer {
  public static class RestoreKey {
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct TokenPrivilege { public uint Count; public long Luid; public uint Attributes; }

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeValueW(string system, string name, out long luid);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TokenPrivilege state, uint length, IntPtr previous, IntPtr returnLength);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern int RegCreateKeyExW(UIntPtr root, string subKey, uint reserved, string cls, uint options, uint access, IntPtr security, out IntPtr key, out uint disposition);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern int RegEnumKeyExW(IntPtr key, uint index, StringBuilder name, ref uint nameLength, IntPtr reserved, IntPtr cls, IntPtr clsLength, IntPtr lastWrite);
    [DllImport("advapi32.dll")]
    static extern int RegCloseKey(IntPtr key);
    [DllImport("ntdll.dll")]
    static extern int NtDeleteKey(IntPtr key);

    static readonly UIntPtr LocalMachine = new UIntPtr(0x80000002u);

    static void Enable(string privilege) {
      IntPtr token;
      // TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY
      if (!OpenProcessToken(GetCurrentProcess(), 0x28, out token)) throw new Win32Exception();
      try {
        TokenPrivilege state = new TokenPrivilege();
        state.Count = 1;
        state.Attributes = 2; // SE_PRIVILEGE_ENABLED
        if (!LookupPrivilegeValueW(null, privilege, out state.Luid)) throw new Win32Exception();
        if (!AdjustTokenPrivileges(token, false, ref state, 0, IntPtr.Zero, IntPtr.Zero)) throw new Win32Exception();
        // ERROR_NOT_ALL_ASSIGNED: the token lacks the privilege.
        int error = Marshal.GetLastWin32Error();
        if (error != 0) throw new Win32Exception(error, privilege + ": " + new Win32Exception(error).Message);
      }
      finally { CloseHandle(token); }
    }

    // REG_OPTION_BACKUP_RESTORE: the access comes from the privileges, not from the key's ACL.
    static IntPtr Open(string subKey, out bool created) {
      Enable("SeBackupPrivilege");
      Enable("SeRestorePrivilege");
      IntPtr key;
      uint disposition;
      int rc = RegCreateKeyExW(LocalMachine, subKey, 0, null, 4, 0, IntPtr.Zero, out key, out disposition);
      if (rc != 0) throw new Win32Exception(rc);
      created = disposition == 1; // REG_CREATED_NEW_KEY
      return key;
    }

    // True when the key was new.
    public static bool Create(string subKey) {
      bool created;
      RegCloseKey(Open(subKey, out created));
      return created;
    }

    // The key and every subkey (Windows adds some under FxProperties).
    public static void Delete(string subKey) {
      bool created;
      IntPtr key = Open(subKey, out created);
      try {
        List<string> children = new List<string>();
        for (uint i = 0; ; i++) {
          StringBuilder name = new StringBuilder(256);
          uint length = (uint)name.Capacity;
          int rc = RegEnumKeyExW(key, i, name, ref length, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
          if (rc == 259) break; // ERROR_NO_MORE_ITEMS
          if (rc != 0) throw new Win32Exception(rc);
          children.Add(name.ToString());
        }
        foreach (string child in children) Delete(subKey + "\\" + child);
        int status = NtDeleteKey(key);
        if (status != 0) throw new Win32Exception("NtDeleteKey failed: 0x" + status.ToString("X8"));
      }
      finally { RegCloseKey(key); }
    }
  }
}
'@

# Add-Type compiles through files in $env:TEMP, which non-admin processes can change between the
# write and the compile: a folder only administrators can write to instead, beside the app folder
# so it never blocks an uninstall.
function Use-RestoreKey {
  if ('GamersEqualizer.RestoreKey' -as [type]) { return }
  $temp = "$ProgramDir.tmp.$([guid]::NewGuid())"
  $saved = $env:TEMP, $env:TMP
  New-Item -ItemType Directory $temp | Out-Null
  try {
    $env:TEMP = $temp
    $env:TMP = $temp
    Add-Type -TypeDefinition $RestoreKeySource
  }
  finally {
    $env:TEMP, $env:TMP = $saved
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
  }
}

# $subKey under HKLM. True when the key was new.
function New-FxKey([string]$subKey) {
  # The privileged create would also make a missing endpoint key (device removed meanwhile).
  $endpoint = $subKey.Substring(0, $subKey.LastIndexOf('\'))
  if (-not (Test-RegKey $endpoint)) { throw "Endpoint not found: HKLM\$endpoint" }
  Use-RestoreKey
  [GamersEqualizer.RestoreKey]::Create($subKey)
}

# Looked up in the parent's subkey names: a key the ACL hides from Administrators still counts.
function Test-RegKey([string]$subKey) {
  $cut = $subKey.LastIndexOf('\')
  $parent = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey.Substring(0, $cut), $false)
  if ($null -eq $parent) { return $false }
  try { @($parent.GetSubKeyNames()) -contains $subKey.Substring($cut + 1) }
  finally { $parent.Dispose() }
}

# Never on a missing key: the privileged open would create it, and every missing parent.
function Remove-FxKey([string]$subKey) {
  if (-not (Test-RegKey $subKey)) { return }
  Use-RestoreKey
  [GamersEqualizer.RestoreKey]::Delete($subKey)
}
