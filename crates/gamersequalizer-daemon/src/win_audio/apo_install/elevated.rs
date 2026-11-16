//! The elevated run: the script written to disk, its SHA-256 checked by the bootstrap, the UAC launch.

use super::*;

/// Oldest Windows build the installer runs on: Windows 10 version 1803, the first that reads
/// composite effect lists. Must match `$MinWindowsBuild` in scripts/apo-install.ps1, which
/// checks again before it changes anything.
const MIN_WINDOWS_BUILD: u32 = 17134;

/// The elevated installer uses files from the app's (user-writable) folder only when its own
/// copy hashes as given here: the DLL it installs and this exe, which it runs with
/// `--probe-mic` to check that Windows starts the mic chain. `endpoints` None: the app alone.
/// Returns the exit code when `run` waits.
pub(super) fn launch_installer(endpoints: Option<&str>, run: ScriptRun) -> Result<Option<u32>, String> {
  // An unreadable build is left to the script's own check.
  if windows_build().is_some_and(|build| build < MIN_WINDOWS_BUILD) {
    return Err(
      tr("The Windows effects need Windows 10 version 1803 or newer. Nothing was installed or changed.").to_string(),
    );
  }
  let dll = bundled_dll().ok_or("APO DLL not found next to the app")?;
  let dll_hash = file_sha256(&dll)?;
  let own_exe = std::env::current_exe().map_err(|e| e.to_string());
  // The installer copies this exe next to the effects (not a dev build: that runs in place)
  // and runs it with --probe-mic.
  let app = match own_exe.and_then(|exe| file_sha256(&exe).map(|hash| (hash, exe))) {
    Ok((hash, exe)) => {
      let in_place = if exe.parent().is_some_and(in_dev_tree) { " -NoAppInstall" } else { "" };
      format!(" -AppExe {} -AppExeSha256 {hash}{in_place}", ps_quote(&exe.to_string_lossy()))
    }
    Err(e) => {
      log::warn!("[Setup] the installer skips the app copy and the microphone check (own exe: {e})");
      String::new()
    }
  };
  let target = match endpoints {
    Some(endpoints) => format!("-Endpoint {}", ps_quote(endpoints)),
    None => "-AppOnly".to_string(),
  };
  let args = format!(
    "{target} -SourceDll {} -SourceDllSha256 {dll_hash}{app} -AppVersion {}{}",
    ps_quote(&dll.to_string_lossy()),
    ps_quote(env!("GAMERSEQUALIZER_VERSION")),
    from_app_arg(run)
  );
  run_script_elevated(SCRIPT_NAME, SCRIPT, &args, "installer", run)
}

/// `-FromApp` keeps the script's window open on errors (Enter closes it); never when silent.
fn from_app_arg(run: ScriptRun) -> &'static str {
  match run {
    ScriptRun::Wait { silent: true } => "",
    _ => " -FromApp",
  }
}

/// Hex SHA-256 of a file (the app's exe is tens of MB: read once per install).
fn file_sha256(path: &Path) -> Result<String, String> {
  let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
  sha256_hex(&data)
}

/// `Capture:{guid}:MFX`, or `:EFX` when it is in MFX already: the slot it moves to.
pub(super) fn switch_arg(guid: &str, in_mfx: bool) -> Result<String, String> {
  let to = if in_mfx { "EFX" } else { "MFX" };
  Ok(format!("{}:{to}", endpoint_args(&[(Flow::Capture, guid)])?))
}

/// `Capture:{guid},Render:{guid}` (one PowerShell array argument, no spaces).
pub(super) fn endpoint_args(targets: &[(Flow, &str)]) -> Result<String, String> {
  let mut parts = Vec::with_capacity(targets.len());
  for (flow, guid) in targets {
    // The GUID came from the device id; reject anything that could break the command line.
    if guid.is_empty() || !guid.chars().all(|c| c.is_ascii_hexdigit() || "{}-".contains(c)) {
      return Err(format!("unexpected endpoint id {guid}"));
    }
    parts.push(format!("{}:{guid}", flow.as_str()));
  }
  Ok(parts.join(","))
}

pub(super) fn uninstall_args(script_dir: Option<&Path>, run: ScriptRun) -> String {
  let mut args = format!("-RemoveBackups{}", from_app_arg(run));
  if let Some(dir) = script_dir {
    args.push_str(" -ScriptDir ");
    args.push_str(&ps_quote(&dir.to_string_lossy()));
  }
  args
}

/// `System32\WindowsPowerShell\v1.0\powershell.exe` from the known folder, never a bare
/// `powershell.exe` (searched in the current / exe folder first) or `%SystemRoot%`
/// (user-overridable).
pub(crate) fn powershell_exe() -> Result<PathBuf, String> {
  let system = known_folder(&FOLDERID_System).ok_or("System32 folder not found")?;
  Ok(system.join("WindowsPowerShell").join("v1.0").join("powershell.exe"))
}

/// Runs a built-in script elevated with `args` (PowerShell syntax, already quoted).
/// The script is too long for the command line (`-EncodedCommand` ~33k chars, limit 32k), so
/// it goes to a user-writable file; the elevated side reads it once into memory and runs that
/// text only if its SHA-256 matches the one computed from `content` here. A same-user process
/// that swaps the file in between gets a refusal, not code running as admin. The bootstrap
/// itself travels in the command line. Module autoload is pinned to the system folder (the
/// user's Documents module folder would otherwise come first and could shadow cmdlets).
/// `ScriptRun::Wait` blocks until the script ends and returns its exit code.
pub(super) fn run_script_elevated(
  name: &str,
  content: &str,
  args: &str,
  what: &str,
  run: ScriptRun,
) -> Result<Option<u32>, String> {
  let content = with_common(content);
  let path = write_script(name, &content)?;
  let hash = sha256_hex(content.as_bytes())?;
  let bootstrap = elevated_bootstrap(&path.to_string_lossy(), &hash, args);
  let params = format!("-NoProfile -ExecutionPolicy Bypass -EncodedCommand {}", encode_command(&bootstrap));
  let exe = powershell_exe()?;
  let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
  let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
  let wait = matches!(run, ScriptRun::Wait { .. });
  let mut info = SHELLEXECUTEINFOW {
    cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
    fMask: if wait { SEE_MASK_NOCLOSEPROCESS } else { Default::default() },
    lpVerb: w!("runas"),
    lpFile: PCWSTR(exe_w.as_ptr()),
    lpParameters: PCWSTR(params_w.as_ptr()),
    nShow: if run == (ScriptRun::Wait { silent: true }) { SW_HIDE.0 } else { SW_SHOWNORMAL.0 },
    ..Default::default()
  };
  // SAFETY: `info` and the NUL-terminated strings it points to live in this frame.
  if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
    remove_file_if_regular(&path);
    if let Some(dir) = path.parent() {
      prune_empty_script_dirs(dir);
    }
    return Err(format!("could not start the {what} (cancelled UAC prompt? {e})"));
  }
  if !wait || info.hProcess.is_invalid() {
    return Ok(None);
  }
  let mut code = 0u32;
  // SAFETY: SEE_MASK_NOCLOSEPROCESS handed us this process handle; it is closed once, here.
  let read = unsafe {
    WaitForSingleObject(info.hProcess, INFINITE);
    let read = GetExitCodeProcess(info.hProcess, &mut code);
    let _ = CloseHandle(info.hProcess);
    read
  };
  read.map_err(|e| format!("could not read the {what}'s exit code: {e}"))?;
  Ok(Some(code))
}

/// PowerShell run by `-EncodedCommand`: check the script file's hash, then run its text.
pub(super) fn elevated_bootstrap(script_path: &str, sha256_hex: &str, args: &str) -> String {
  let path = ps_quote(script_path);
  // A hidden (silent) run has nobody to press Enter.
  let pause = if args.contains("-FromApp") { "Read-Host 'Press Enter to close'; " } else { "" };
  [
    "$env:PSModulePath = [IO.Path]::Combine([Environment]::GetFolderPath('System'), 'WindowsPowerShell\\v1.0\\Modules')"
      .to_string(),
    format!("$c = [IO.File]::ReadAllText({path})"),
    "$h = -join ([Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($c)) | ForEach-Object { $_.ToString('x2') })".to_string(),
    format!(
      "if ($h -ne '{sha256_hex}') {{ Write-Host 'The script was changed after the app wrote it; not running it.' -ForegroundColor Red; {pause}exit 1 }}"
    ),
    // Dot-sourced: its top-level variables must land in the script scope, which the scripts'
    // functions use as `$script:X` (`&` would give them a child scope; `$script:X` was then null).
    format!(". ([scriptblock]::Create($c)) {args}"),
  ]
  .join("\n")
}

/// `script` with `apo-common.ps1` in place of the line that loads it from disk.
pub(super) fn with_common(script: &str) -> String {
  script.replacen(COMMON_LINE, COMMON_SCRIPT, 1)
}

/// `-EncodedCommand` takes base64 of UTF-16LE.
pub(super) fn encode_command(script: &str) -> String {
  let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
  base64(&bytes)
}

pub(super) fn base64(bytes: &[u8]) -> String {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let at = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
    let n = (at(0) << 16) | (at(1) << 8) | at(2);
    for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
      // `n >> shift & 63` is always < 64.
      let symbol = ALPHABET.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'=');
      out.push(if i <= chunk.len() { char::from(symbol) } else { '=' });
    }
  }
  out
}

pub(super) fn sha256_hex(data: &[u8]) -> Result<String, String> {
  let mut digest = [0u8; 32];
  // SAFETY: the SHA-256 pseudo-handle (Windows 10+) needs no open/close; buffers are local.
  let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut digest) };
  if status.is_err() {
    return Err(format!("SHA-256 failed (0x{:08x})", status.0));
  }
  Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// PowerShell single-quoted literal: nothing inside is expanded, `'` is doubled.
pub(crate) fn ps_quote(s: &str) -> String {
  format!("'{}'", s.replace('\'', "''"))
}

pub(super) fn scripts_dir() -> Option<PathBuf> {
  let base = std::env::var_os("LOCALAPPDATA")?;
  Some(PathBuf::from(base).join("GamersEqualizer").join("scripts"))
}

/// Writes a built-in script fresh before every run, so an old or edited copy is never used.
/// The folder is user-writable; `run_script_elevated` checks the hash before running it.
fn write_script(name: &str, content: &str) -> Result<PathBuf, String> {
  let dir = scripts_dir().ok_or("LOCALAPPDATA is not set")?;
  std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
  let path = dir.join(name);
  std::fs::write(&path, content).map_err(|e| format!("could not write {}: {e}", path.display()))?;
  Ok(path)
}

/// Removes temporary scripts staged in `%LOCALAPPDATA%\GamersEqualizer\scripts` and drops
/// empty directories without deleting unrelated sibling folders (such as diagnostic logs).
pub(crate) fn cleanup_staged_scripts() {
  if let Some(dir) = scripts_dir() {
    cleanup_staged_scripts_in(&dir);
  }
}

pub(super) fn cleanup_staged_scripts_in(dir: &Path) {
  if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
    return;
  }
  for name in [SCRIPT_NAME, UNINSTALL_SCRIPT_NAME] {
    remove_file_if_regular(&dir.join(name));
  }
  prune_empty_script_dirs(dir);
}

fn remove_file_if_regular(file: &Path) {
  if std::fs::symlink_metadata(file).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file()) {
    let _ = std::fs::remove_file(file);
  }
}

fn prune_empty_script_dirs(dir: &Path) {
  if std::fs::symlink_metadata(dir).is_ok_and(|m| !m.file_type().is_symlink()) {
    let _ = std::fs::remove_dir(dir);
  }
  if let Some(parent) = dir.parent()
    && std::fs::symlink_metadata(parent).is_ok_and(|m| !m.file_type().is_symlink())
  {
    let _ = std::fs::remove_dir(parent);
  }
}
