//! What the service's supervised worker threads share: an error that repeats is logged once, and
//! the APO shared memory is opened with retries instead of ending the thread (the loopback thread
//! carries the link to the APO; ended, the APO ran without settings until a restart).

use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use std::time::Duration;

const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);

/// Remembers the last error so the same one in a row is logged once, not every retry.
#[derive(Default)]
pub struct RepeatFilter(Option<String>);

impl RepeatFilter {
  /// True when `err` differs from the error before it: log it then.
  pub fn is_new(&mut self, err: &str) -> bool {
    if self.0.as_deref() == Some(err) {
      return false;
    }
    self.0 = Some(err.to_owned());
    true
  }

  /// The work succeeded: the next error is new again.
  pub fn reset(&mut self) {
    self.0 = None;
  }
}

/// Opens the APO shared memory, retrying with a growing wait (capped) while `running` says so.
/// None only when the thread is asked to stop first.
pub fn open_shm(tag: &str, running: impl Fn() -> bool) -> Option<SharedMemoryClient> {
  let mut seen = RepeatFilter::default();
  let mut wait = FIRST_WAIT;
  while running() {
    match SharedMemoryClient::open_or_create() {
      Ok(shm) => return Some(shm),
      Err(e) => {
        if seen.is_new(&e) {
          log::warn!("[{tag}] shared memory unavailable, retrying: {e}");
        }
      }
    }
    std::thread::sleep(wait);
    wait = (wait * 2).min(MAX_WAIT);
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_same_error_in_a_row_is_new_once() {
    let mut seen = RepeatFilter::default();
    assert!(seen.is_new("device gone"));
    assert!(!seen.is_new("device gone"));
    assert!(seen.is_new("access denied"));
    assert!(seen.is_new("device gone"));
    seen.reset();
    assert!(seen.is_new("device gone"));
  }
}
