//! Which daemon may drive the system-wide APO. The APO (one per audio device, in audiodg)
//! serves whoever sits at the console, so only the daemon of the active console session
//! writes settings into its Global mapping. Others (another Windows user switched away, a
//! Remote Desktop session) leave it alone; without a live owner the APO passes audio through.

use log::info;
use std::time::{Duration, Instant};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Threading::GetCurrentProcessId;

/// Fast user switching changes the console session; a second is quick enough to follow it.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct ConsoleOwner {
  checked_at: Option<Instant>,
  owner: bool,
}

impl ConsoleOwner {
  /// This process runs in the session at the console (cached for `CHECK_INTERVAL`).
  pub fn is_owner(&mut self) -> bool {
    if self.checked_at.is_none_or(|t| t.elapsed() >= CHECK_INTERVAL) {
      self.checked_at = Some(Instant::now());
      let owner = at_console();
      if owner != self.owner {
        info!("[ConsoleOwner] {}", if owner { "this session is at the console: driving the APO" } else {
          "another session is at the console: leaving the APO alone"
        });
        self.owner = owner;
      }
    }
    self.owner
  }
}

fn at_console() -> bool {
  let mut mine = 0u32;
  // SAFETY: plain Win32 queries with a valid out pointer.
  let (active, ok) = unsafe { (WTSGetActiveConsoleSessionId(), ProcessIdToSessionId(GetCurrentProcessId(), &mut mine)) };
  // Unknown session: behave as before (drive the APO) rather than switch it off for everyone.
  ok.is_err() || mine == active
}
