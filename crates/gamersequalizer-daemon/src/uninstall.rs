//! "Uninstall from Windows" (Settings): removes everything the app registered in the system.
//!
//! - Microphone / output effects (APO): elevated `scripts/apo-uninstall.ps1` takes our classes
//!   off every audio device, unregisters them and deletes the DLL and the install backups.
//! - Start with Windows (`HKCU\...\Run`) and the notification registration (`notify.rs`).
//! - Optionally the user's settings and profiles (`%APPDATA%\GamersEqualizer`, legacy
//!   `%APPDATA%\OpenSonar`). This process and the background service keep files open there
//!   (the log), so a hidden helper deletes them once every app process has exited.
//!
//! The app folder itself (portable exe) is the user's to delete; a running exe can't remove itself.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use log::{info, warn};

use crate::win_audio::apo_install;

const DATA_DIRS: [&str; 2] = ["GamersEqualizer", "OpenSonar"];
/// No console window for the helper.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long the helper waits for the app processes to exit before deleting anyway.
const WAIT_FOR_EXIT_SECS: u32 = 20;

/// Does everything that can fail before the app exits. On `Err` nothing irreversible has
/// happened yet when the APO step failed (e.g. the UAC prompt was cancelled); the caller shows it
/// and keeps running. On `Ok` the caller stops the background service and exits.
pub fn uninstall_all(remove_user_data: bool) -> Result<(), String> {
  // First, so a cancelled UAC prompt leaves the rest untouched.
  if apo_install::is_registered() {
    apo_install::launch_uninstall()?;
    info!("[Uninstall] APO uninstaller started");
  }
  if let Err(e) = crate::autostart::set_autostart(false) {
    // Not fatal: the other parts still go; the user sees the entry in Task Manager > Startup.
    warn!("[Uninstall] could not remove autostart: {e}");
  }
  if let Err(e) = crate::notify::unregister() {
    warn!("[Uninstall] {e}");
  }
  if remove_user_data {
    spawn_data_cleanup()?;
  }
  Ok(())
}

fn spawn_data_cleanup() -> Result<(), String> {
  let app_data = std::env::var_os("APPDATA").map(PathBuf::from).ok_or("APPDATA is not set")?;
  let dirs: Vec<PathBuf> = DATA_DIRS.iter().map(|d| app_data.join(d)).filter(|p| p.exists()).collect();
  if dirs.is_empty() {
    return Ok(());
  }
  let exe_stem = std::env::current_exe()
    .ok()
    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
    .ok_or("could not determine the app's process name")?;
  let script = cleanup_script(&exe_stem, &dirs);
  Command::new("powershell.exe")
    .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
    .creation_flags(CREATE_NO_WINDOW)
    .spawn()
    .map_err(|e| format!("could not start the data cleanup: {e}"))?;
  info!("[Uninstall] user data cleanup scheduled for {dirs:?}");
  Ok(())
}

/// Waits for all `exe_stem` processes (this window and the background service), then deletes
/// the folders, retrying a few times while a file is still held open.
fn cleanup_script(exe_stem: &str, dirs: &[PathBuf]) -> String {
  let quoted: Vec<String> = dirs.iter().map(|d| ps_quote(&d.to_string_lossy())).collect();
  format!(
    "Wait-Process -Name {} -Timeout {WAIT_FOR_EXIT_SECS} -ErrorAction SilentlyContinue; \
     foreach ($p in @({})) {{ for ($i = 0; $i -lt 10 -and (Test-Path -LiteralPath $p); $i++) {{ \
     Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue; Start-Sleep -Seconds 1 }} }}",
    ps_quote(exe_stem),
    quoted.join(", ")
  )
}

/// PowerShell single-quoted literal: nothing inside is expanded, `'` is doubled.
fn ps_quote(s: &str) -> String {
  format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paths_are_single_quoted_literals() {
    let script = cleanup_script("game's eq", &[PathBuf::from(r"C:\Users\O'Neil\AppData\Roaming\GamersEqualizer")]);
    assert!(script.contains("-Name 'game''s eq'"));
    assert!(script.contains(r"@('C:\Users\O''Neil\AppData\Roaming\GamersEqualizer')"));
    assert!(!script.contains('"'));
  }
}
