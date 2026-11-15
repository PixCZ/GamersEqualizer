//! `--install` / `--uninstall` without the window, for package managers (winget) and scripts.
//! `--silent` hides the elevated script's window and never waits for a key. The process exits
//! with the script's exit code, 1 when it could not be started (cancelled UAC prompt).

use std::sync::Arc;

use log::{error, info, warn};

use crate::win_audio::apo_install::{self, Flow, ScriptRun};
use crate::win_audio::chatmix::physical_endpoint_id;
use crate::win_audio::endpoint_lock::{EndpointLockManager, EndpointLockState};

const INSTALL_ARG: &str = "--install";
const UNINSTALL_ARG: &str = "--uninstall";
const SILENT_ARG: &str = "--silent";
const START_FAILED: i32 = 1;

/// Some(exit code) when `args` asked for a setup action, None to start the app normally.
pub fn run(args: &[String]) -> Option<i32> {
  let has = |flag: &str| args.iter().any(|a| a == flag);
  let silent = has(SILENT_ARG);
  let result = if has(UNINSTALL_ARG) {
    uninstall(silent)
  } else if has(INSTALL_ARG) {
    install(silent)
  } else {
    return None;
  };
  Some(match result {
    Ok(code) => {
      info!("[SetupCli] script finished with exit code {code}");
      // Exit codes above i32::MAX (NTSTATUS crashes) keep their bits.
      code as i32
    }
    Err(e) => {
      error!("[SetupCli] {e}");
      START_FAILED
    }
  })
}

/// Update: the effects go where they already are. First install: on the microphone and output
/// in use. No audio device at all (a test VM): the app alone.
fn install(silent: bool) -> Result<u32, String> {
  // A running older copy would hold the exe the installer replaces.
  crate::takeover::end_other_copies();
  let mut targets = apo_install::endpoints_with_effects();
  if targets.is_empty() {
    targets = devices_in_use();
  }
  info!("[SetupCli] installing on {targets:?} (silent: {silent})");
  let refs: Vec<(Flow, &str)> = targets.iter().map(|(flow, guid)| (*flow, guid.as_str())).collect();
  apo_install::install_and_wait(&refs, silent)
}

fn uninstall(silent: bool) -> Result<u32, String> {
  // The uninstaller removes the exe; a running copy would keep it (and its effects' settings) alive.
  crate::takeover::end_other_copies();
  if let Err(e) = crate::autostart::set_autostart(false) {
    warn!("[SetupCli] could not remove autostart: {e}");
  }
  if let Err(e) = crate::notify::unregister() {
    warn!("[SetupCli] {e}");
  }
  info!("[SetupCli] uninstalling (silent: {silent})");
  apo_install::launch_uninstall(ScriptRun::Wait { silent })?
    .ok_or_else(|| "the uninstaller gave no exit code".to_string())
}

/// The physical microphone and output in use (a virtual default such as a Sonar or VB-Cable
/// device is skipped for the first real one), as (flow, endpoint GUID).
fn devices_in_use() -> Vec<(Flow, String)> {
  let manager = match EndpointLockManager::new(Arc::new(EndpointLockState::new())) {
    Ok(manager) => manager,
    Err(e) => {
      warn!("[SetupCli] cannot list audio devices: {e}");
      return Vec::new();
    }
  };
  let mic = physical_endpoint_id(&manager.enumerate_capture_devices(), manager.get_default_capture_id().as_deref());
  let out = physical_endpoint_id(&manager.enumerate_playback_devices(), manager.get_default_output_id().as_deref());
  [(Flow::Capture, mic), (Flow::Render, out)]
    .into_iter()
    .filter_map(|(flow, id)| Some((flow, apo_install::endpoint_guid_of(&id?)?)))
    .collect()
}
