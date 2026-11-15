use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use windows::Win32::Foundation::STILL_ACTIVE;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
  GetCurrentThreadId, GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
  QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
  DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetMessageW, GetWindowThreadProcessId, MSG, PM_NOREMOVE, PeekMessageW,
  PostThreadMessageW, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};
use windows::core::PWSTR;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameForegroundEvent {
  pub pid: u32,
  pub exe_name: String,
  pub full_path: String,
}

/// Where the hook callback (a plain `extern "system" fn`, no context pointer) sends window handles.
static HOOK_SENDER: Mutex<Option<Sender<isize>>> = Mutex::new(None);

/// Foreground-window watcher. The hook is out-of-context, so Windows delivers its events
/// through the message queue of the thread that installed it: that thread must pump
/// messages. It gets its own thread with a message loop (the daemon's main loop only sleeps,
/// so a hook installed there never fired).
pub struct GameDetector {
  hook_thread_id: u32,
  hook_thread: Option<JoinHandle<()>>,
  rx: Receiver<GameForegroundEvent>,
  current_game: Arc<Mutex<Option<GameForegroundEvent>>>,
}

impl GameDetector {
  pub fn start() -> Result<Self, String> {
    let (raw_tx, raw_rx) = channel::<isize>();
    let (event_tx, event_rx) = channel::<GameForegroundEvent>();
    let (ready_tx, ready_rx) = channel::<Result<u32, String>>();
    let current_game = Arc::new(Mutex::new(None));

    *HOOK_SENDER.lock().unwrap_or_else(PoisonError::into_inner) = Some(raw_tx);
    let hook_thread = std::thread::Builder::new()
      .name("GamersEqualizer_GameHook".into())
      .spawn(move || run_hook_thread(ready_tx))
      .map_err(|e| format!("game hook thread: {e}"))?;
    let hook_thread_id = match ready_rx.recv() {
      Ok(Ok(id)) => id,
      Ok(Err(e)) => return Err(e),
      Err(_) => return Err("game hook thread exited before reporting".to_string()),
    };

    let current_clone = Arc::clone(&current_game);

    // Background worker to resolve HWND -> PID -> Image name without blocking the Win32 hook
    std::thread::spawn(move || {
      let mut last_pid = 0u32;

      while let Ok(raw_hwnd) = raw_rx.recv() {
        let hwnd = HWND(raw_hwnd as *mut _);
        let mut pid = 0u32;
        // SAFETY: a window handle the hook reported (a closed window only makes the call fail);
        // `pid` is a local.
        let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 || pid == last_pid {
          continue;
        }
        last_pid = pid;

        if let Some((exe_name, full_path)) = resolve_process_name(pid) {
          let event = GameForegroundEvent { pid, exe_name, full_path };

          {
            let mut guard = match current_clone.lock() {
              Ok(g) => g,
              Err(p) => p.into_inner(),
            };
            *guard = Some(event.clone());
          }

          let _ = event_tx.send(event);
        }
      }
    });

    Ok(Self { hook_thread_id, hook_thread: Some(hook_thread), rx: event_rx, current_game })
  }

  pub fn poll_event(&self) -> Option<GameForegroundEvent> {
    self.rx.try_recv().ok()
  }

  pub fn current(&self) -> Option<GameForegroundEvent> {
    let guard = match self.current_game.lock() {
      Ok(g) => g,
      Err(p) => p.into_inner(),
    };
    guard.clone()
  }
}

impl Drop for GameDetector {
  fn drop(&mut self) {
    // SAFETY: posts to the hook thread's own id, which stays valid until it is joined right after.
    unsafe {
      // Ends the message loop; the hook thread unhooks itself.
      if PostThreadMessageW(self.hook_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_ok()
        && let Some(t) = self.hook_thread.take()
      {
        let _ = t.join();
      }
    }
    *HOOK_SENDER.lock().unwrap_or_else(PoisonError::into_inner) = None;
  }
}

/// Installs the hook, reports this thread's id, then pumps messages until WM_QUIT.
fn run_hook_thread(ready: Sender<Result<u32, String>>) {
  // SAFETY: plain Win32 calls on this thread; `msg` is a local.
  unsafe {
    let mut msg = MSG::default();
    // Creates the message queue now, so a WM_QUIT posted right after "ready" isn't lost.
    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    let hook = SetWinEventHook(
      EVENT_SYSTEM_FOREGROUND,
      EVENT_SYSTEM_FOREGROUND,
      None,
      Some(win_event_proc),
      0,
      0,
      WINEVENT_OUTOFCONTEXT,
    );
    if hook.is_invalid() {
      let _ = ready.send(Err("Failed to register SetWinEventHook".to_string()));
      return;
    }
    let _ = ready.send(Ok(GetCurrentThreadId()));
    // 0 = WM_QUIT, -1 = error: both end the loop.
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
      DispatchMessageW(&msg);
    }
    let _ = UnhookWinEvent(hook);
  }
}

unsafe extern "system" fn win_event_proc(
  _h_win_event_hook: HWINEVENTHOOK,
  event: u32,
  hwnd: HWND,
  _id_object: i32,
  _id_child: i32,
  _id_event_thread: u32,
  _dwms_event_time: u32,
) {
  if event == EVENT_SYSTEM_FOREGROUND
    && !hwnd.is_invalid()
    && let Some(tx) = HOOK_SENDER.lock().unwrap_or_else(PoisonError::into_inner).as_ref()
  {
    let _ = tx.send(hwnd.0 as isize);
  }
}

/// False once the process has exited (or can no longer be opened at all).
pub fn is_process_running(pid: u32) -> bool {
  // SAFETY: handle is checked and closed; the out parameter is a local.
  unsafe {
    let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
      return false;
    };
    let mut code = 0u32;
    let ok = GetExitCodeProcess(handle, &mut code).is_ok();
    let _ = CloseHandle(handle);
    ok && code == STILL_ACTIVE.0 as u32
  }
}

/// (exe name, full Win32 path like `D:\SteamLibrary\steamapps\common\...\division2.exe`).
fn resolve_process_name(pid: u32) -> Option<(String, String)> {
  // SAFETY: the process handle is checked and closed below; the name buffer is a local array whose length goes in
  // `len`.
  unsafe {
    let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    if handle.is_invalid() {
      return None;
    }

    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(handle);

    if ok && len > 0 {
      let full_path = String::from_utf16_lossy(buf.get(..len as usize).unwrap_or_default());
      let exe_name = full_path.rsplit('\\').next().unwrap_or(&full_path).to_string();
      return Some((exe_name, full_path));
    }
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_hook_thread_starts_and_stops() {
    // Regression: the hook lived on a thread without a message loop and never fired; now
    // it has its own thread, which must also shut down cleanly (drop must not hang).
    let detector = GameDetector::start().unwrap();
    assert!(detector.hook_thread.as_ref().is_some_and(|t| !t.is_finished()));
    drop(detector);
  }
}
