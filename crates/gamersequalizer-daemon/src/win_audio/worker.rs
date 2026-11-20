//! What the service's supervised worker threads share: an error that repeats is logged once, and
//! the APO shared memory is opened with retries instead of ending the thread (the reference thread
//! carries the link to the APO; ended, the APO ran without settings until a restart).

use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use log::debug;
use std::time::Duration;

const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Service loop and reference thread while Automatic ChatMix's mix mode moves a game's volume:
/// 25 steps a second (the fast slew is 40 dB/s), not 20 minus the work.
const MIX_TICK: Duration = Duration::from_millis(40);
/// While something must follow at 20 Hz (the window's sliders, mix mode reading the reference).
pub const TICK: Duration = Duration::from_millis(50);
/// Idle, so a laptop's CPU can stay asleep: 4 wakeups a second. Still far inside the APO's
/// `OWNER_TIMEOUT` (3 s) and the 1-3 s windows the beats and the DSP watch are judged over.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// Why a loop must tick soon, and how soon that reason needs it.
pub type Busy = (&'static str, Duration);

/// Remembers the last error so the same one in a row is logged once, not every retry.
#[derive(Default)]
pub struct RepeatFilter(Option<String>);

/// Whether a loop ticks fast and why, as last logged (None before the first tick).
#[derive(Default)]
pub struct IdleLog(Option<Option<Busy>>);

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

/// The wait between two ticks of a service loop. `mix_fast`: `ducking::mix_fast`; `busy`: what
/// the caller has that must follow (`busy_reason`), None = idle.
pub fn tick_wait(mix_fast: bool, busy: Option<Busy>) -> Duration {
  if mix_fast { MIX_TICK } else { busy.map_or(IDLE_TICK, |(_, tick)| tick) }
}

/// Of the reasons that hold, the one that needs the shortest tick; None: the loop is idle.
pub fn busy_reason(reasons: &[(bool, &'static str, Duration)]) -> Option<Busy> {
  reasons.iter().filter(|(holds, ..)| *holds).min_by_key(|(_, _, tick)| *tick).map(|&(_, reason, tick)| (reason, tick))
}

impl IdleLog {
  /// True when idle. One `[Idle]` line per change, so the log says why `loop_name` ticks fast or slow.
  pub fn idle(&mut self, loop_name: &str, busy: Option<Busy>) -> bool {
    if self.0 != Some(busy) {
      self.0 = Some(busy);
      match busy {
        Some((reason, tick)) => debug!("[Idle] {loop_name}: busy ({reason}), {} ms ticks", tick.as_millis()),
        None => debug!("[Idle] {loop_name}: idle, {} ms ticks", IDLE_TICK.as_millis()),
      }
    }
    busy.is_none()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_reason_that_needs_the_shortest_tick_sets_the_pace() {
    let sync = Duration::from_millis(100);
    assert_eq!(busy_reason(&[(false, "window open", TICK), (false, "app records", sync)]), None);
    let reasons = [(true, "app records", sync), (true, "mix mode", TICK), (false, "window open", MIX_TICK)];
    assert_eq!(busy_reason(&reasons), Some(("mix mode", TICK)));
    assert_eq!(busy_reason(&[(true, "app records", sync), (true, "ChatMix", sync)]), Some(("app records", sync)));
    assert_eq!(busy_reason(&[]), None);
  }

  #[test]
  fn idle_log_reports_idle_only_without_a_reason() {
    let mut log = IdleLog::default();
    assert!(log.idle("loop", None));
    assert!(!log.idle("loop", Some(("window open", TICK))));
    assert_eq!(log.0, Some(Some(("window open", TICK))), "the change is remembered, so it is logged once");
  }

  #[test]
  fn a_moving_reduction_ticks_fastest_and_idle_slowest() {
    let sync = Duration::from_millis(100);
    assert_eq!(tick_wait(true, None), MIX_TICK, "mix mode moving wins over idle");
    assert_eq!(tick_wait(true, Some(("app records", sync))), MIX_TICK);
    assert_eq!(tick_wait(false, Some(("window open", TICK))), TICK);
    assert_eq!(tick_wait(false, Some(("app records", sync))), sync);
    assert_eq!(tick_wait(false, None), IDLE_TICK);
  }

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
