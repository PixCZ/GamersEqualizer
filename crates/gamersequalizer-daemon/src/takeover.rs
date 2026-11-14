//! A newer copy of the app ends the older one that is running, so "unpack the new version and
//! start it" is all an update takes: this copy then offers to update the installation
//! (`apo_install`) and continues from Program Files.
//!
//! "Older" = another process with this exe's file name whose file is a different one and was
//! not built later. It is asked first (window closed, service told to stop through the
//! shared memory, so both save their state); what is still running after that is terminated:
//! a service of another shared-memory version cannot be asked at all.

use log::{info, warn};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use gamersequalizer_common::shm::SHM_NAME;
use gamersequalizer_mic_vst::SharedMemoryClient;
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::ProcessStatus::EnumProcesses;
use windows::Win32::System::Threading::{
  GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
  PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// Room for every process id on the machine.
const MAX_PROCESSES: usize = 4096;
/// How long the older copy gets to close its window and stop its service by itself.
const ASKED_WAIT: Duration = Duration::from_secs(3);
/// How long a terminated process may take to be gone, ms.
const TERMINATE_WAIT_MS: u32 = 3000;
const POLL: Duration = Duration::from_millis(100);

/// Ends every older copy of the app that is running; true when there was one.
pub fn replace_older_copies() -> bool {
  // The path as Windows resolves it, like the other processes' below: `current_exe` keeps
  // the path as started (a mapped drive, a short name) and this app would not know itself.
  // SAFETY: no arguments, cannot fail.
  let own_pid = unsafe { GetCurrentProcessId() };
  let Some(this) = image_path(own_pid).or_else(|| std::env::current_exe().ok()) else {
    return false;
  };
  let older = older_copies(&this);
  if older.is_empty() {
    return false;
  }
  info!("[GamersEqualizer] an older copy of the app is running; this one takes over");
  // One service runs at a time. With another process of this very file around it is this
  // copy's own (the app is running, and only a leftover of an older copy is in the way).
  let own_service = other_processes(&this).iter().any(|(_, path)| same_file(path, &this));
  end_copies(&older, !own_service, || older_copies(&this));
  true
}

/// Ends every other running copy of the app (window and service), whatever its build:
/// `--uninstall` does this first, so nothing keeps running from the files it removes.
pub fn end_other_copies() {
  // SAFETY: no arguments, cannot fail.
  let own_pid = unsafe { GetCurrentProcessId() };
  let Some(this) = image_path(own_pid).or_else(|| std::env::current_exe().ok()) else {
    return;
  };
  let others = other_processes(&this);
  if !others.is_empty() {
    end_copies(&others, true, || other_processes(&this));
  }
}

/// Asks `copies` to quit (window closed, service told to stop when `ask_service`), waits a
/// little, then terminates what `still_running` still lists.
fn end_copies(copies: &[(u32, PathBuf)], ask_service: bool, still_running: impl Fn() -> Vec<(u32, PathBuf)>) {
  // The window first: its watchdog would start the service again.
  close_window_of(copies);
  if ask_service {
    match SharedMemoryClient::open_existing(SHM_NAME) {
      Ok(shm) => shm.layout().header.shutdown_daemon_signal.store(true, Ordering::SeqCst),
      Err(e) => info!("[GamersEqualizer] the other service cannot be asked to stop ({e}); it will be ended"),
    }
  }
  let asked = Instant::now();
  while asked.elapsed() < ASKED_WAIT && !still_running().is_empty() {
    std::thread::sleep(POLL);
  }
  for (pid, path) in still_running() {
    match terminate(pid) {
      Ok(()) => info!("[GamersEqualizer] ended the other copy {} (pid {pid})", path.display()),
      Err(e) => warn!("[GamersEqualizer] could not end the other copy {} (pid {pid}): {e}", path.display()),
    }
  }
}

/// (pid, exe file) of each running copy of the app that this one replaces.
fn older_copies(this: &Path) -> Vec<(u32, PathBuf)> {
  let Some(this_built) = built(this) else {
    return Vec::new();
  };
  let others = other_processes(this).into_iter();
  others.filter(|(_, path)| !same_file(path, this) && replaces(this_built, built(path))).collect()
}

/// Asks the app window to close when it belongs to one of `copies`.
fn close_window_of(copies: &[(u32, PathBuf)]) {
  let Some(window) = crate::tray::find_window() else {
    return;
  };
  let mut owner = 0u32;
  // SAFETY: a window handle from FindWindow; the out value is local.
  unsafe {
    GetWindowThreadProcessId(window, Some(&mut owner));
  }
  if copies.iter().any(|(pid, _)| *pid == owner) {
    crate::tray::close_window();
  }
}

/// A copy built at `this_built` replaces one built at `other`: not a later build (an
/// unreadable file counts as older: it was deleted under a running process).
fn replaces(this_built: SystemTime, other: Option<SystemTime>) -> bool {
  other.is_none_or(|other| other <= this_built)
}

fn built(exe: &Path) -> Option<SystemTime> {
  std::fs::metadata(exe).ok()?.modified().ok()
}

/// (pid, exe file) of every other process with this exe's file name.
fn other_processes(this: &Path) -> Vec<(u32, PathBuf)> {
  let Some(name) = this.file_name() else {
    return Vec::new();
  };
  let mut pids = vec![0u32; MAX_PROCESSES];
  let mut bytes = 0u32;
  let size = u32::try_from(std::mem::size_of_val(pids.as_slice())).unwrap_or(0);
  // SAFETY: the buffer is `size` bytes; `bytes` says how many were filled.
  if unsafe { EnumProcesses(pids.as_mut_ptr(), size, &mut bytes) }.is_err() {
    return Vec::new();
  }
  // SAFETY: no arguments, cannot fail.
  let own = unsafe { GetCurrentProcessId() };
  let count = bytes as usize / std::mem::size_of::<u32>();
  pids
    .iter()
    .take(count)
    .filter(|&&pid| pid != 0 && pid != own)
    .filter_map(|&pid| image_path(pid).map(|path| (pid, path)))
    .filter(|(_, path)| path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)))
    .collect()
}

/// Windows paths compare without case.
fn same_file(a: &Path, b: &Path) -> bool {
  a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

fn image_path(pid: u32) -> Option<PathBuf> {
  // SAFETY: the handle is closed before returning; the buffer is local.
  unsafe {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(handle);
    let path = buf.get(..len as usize).filter(|p| ok && !p.is_empty())?;
    Some(PathBuf::from(String::from_utf16_lossy(path)))
  }
}

fn terminate(pid: u32) -> windows::core::Result<()> {
  // SAFETY: the handle is closed before returning.
  unsafe {
    let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid)?;
    let result = TerminateProcess(handle, 0);
    if result.is_ok() {
      // Its exe and the service's single-instance mutex are free once it is gone.
      let _ = WaitForSingleObject(handle, TERMINATE_WAIT_MS);
    }
    let _ = CloseHandle(handle);
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_copy_replaces_older_and_equal_builds_only() {
    let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    assert!(replaces(at(200), Some(at(100))), "older build");
    assert!(replaces(at(200), Some(at(200))), "same build from another folder");
    assert!(!replaces(at(200), Some(at(300))), "a newer build keeps running");
    assert!(replaces(at(200), None), "its file is gone");
  }

  #[test]
  fn paths_compare_without_case() {
    assert!(same_file(
      Path::new(r"C:\Program Files\GamersEqualizer\gamersequalizer.exe"),
      Path::new(r"c:\program files\gamersequalizer\GamersEqualizer.EXE")
    ));
    assert!(!same_file(Path::new(r"C:\a\gamersequalizer.exe"), Path::new(r"C:\b\gamersequalizer.exe")));
  }
}
