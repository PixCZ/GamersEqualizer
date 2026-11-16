//! Handover to the installed copy, the import after the window closes, and the service mutex.

use super::*;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const DETACHED_PROCESS: u32 = 0x00000008;

/// Starting the installed copy failed in this process: it keeps running from here.
static HANDOVER_FAILED: AtomicBool = AtomicBool::new(false);

/// Starts the installed copy with `args` in place of this process, when there is one to hand
/// over to (`handover_target`); true when it was started and this process should end. The
/// autostart entry follows. `NO_HANDOVER_ARG` keeps a path mix-up from looping.
pub(crate) fn hand_over_to_installed(args: &[String]) -> bool {
  if HANDOVER_FAILED.load(Ordering::Relaxed) {
    return false;
  }
  let Some(installed) = win_audio::apo_install::handover_target() else {
    return false;
  };
  autostart::repoint();
  let started = Command::new(&installed).args(args).arg(NO_HANDOVER_ARG).creation_flags(DETACHED_PROCESS).spawn();
  match started {
    Ok(_) => {
      info!("[GamersEqualizer] handing over to the installed app: {installed:?}");
      true
    }
    Err(e) => {
      error!("[GamersEqualizer] could not start the installed app {installed:?}, running from here: {e}");
      HANDOVER_FAILED.store(true, Ordering::Relaxed);
      false
    }
  }
}

/// Imports the `only` parts of `path` after this window has closed (the caller closes it) and
/// the service has stopped, then starts the app again with the result.
pub(crate) fn import_after_close(state: &AppState, path: PathBuf, only: settings_transfer::Selection) {
  *PENDING_IMPORT.lock().unwrap_or_else(|e| e.into_inner()) = Some((path, only));
  state.signal_daemon_shutdown();
}

/// An import waits for the window to close: the service must stay stopped.
pub(crate) fn import_pending() -> bool {
  PENDING_IMPORT.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// The window's part of an install: true once the installer has put this app into Program
/// Files. The caller closes the window (and no longer restarts the service); `run_gui_mode`
/// then starts the installed app. This copy's service is told to stop: the installed one
/// would exit as a duplicate.
pub(crate) fn installed_copy_ready(state: &AppState) -> bool {
  if MOVE_TO_INSTALLED.load(Ordering::Relaxed) {
    return true;
  }
  if HANDOVER_FAILED.load(Ordering::Relaxed) || win_audio::apo_install::handover_target().is_none() {
    return false;
  }
  state.signal_daemon_shutdown();
  MOVE_TO_INSTALLED.store(true, Ordering::Relaxed);
  true
}

/// True while a background service process lives (it holds the mutex until it exits).
pub(crate) fn daemon_running() -> bool {
  // Open, never create: a mutex created here for a moment made a service starting at that
  // instant see it as taken and exit.
  // SAFETY: a static name; the handle is closed below.
  let Ok(mutex) =
    (unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!(r"Local\GamersEqualizer_Daemon_Mutex")) })
  else {
    return false;
  };
  // SAFETY: the handle OpenMutexW returned above, closed once.
  unsafe {
    let _ = CloseHandle(mutex);
  }
  true
}

/// Check if daemon mutex exists, and start detached background process if not.
/// Also called periodically by the UI: the mutex disappears with the daemon process (crash,
/// kill), unlike the `daemon_active` flag in shared memory, which a killed daemon leaves set.
pub(crate) fn ensure_daemon_running() {
  if !daemon_running()
    && let Ok(exe_path) = std::env::current_exe()
  {
    info!("[GamersEqualizer] Spawning detached background daemon: {:?}", exe_path);
    let _ = Command::new(&exe_path).arg("--daemon").creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn();
    std::thread::sleep(Duration::from_millis(150));
  }
}
