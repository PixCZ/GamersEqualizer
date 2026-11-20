//! What the service's supervised worker threads share: an error that repeats is logged once, and
//! the APO shared memory is opened with retries instead of ending the thread (the reference thread
//! carries the link to the APO; ended, the APO ran without settings until a restart).

use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use log::debug;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Service loop and reference thread while Automatic ChatMix's mix mode moves a game's volume:
/// 25 steps a second (the fast slew is 40 dB/s), not 20 minus the work.
const MIX_TICK: Duration = Duration::from_millis(40);
/// While something must follow at 20 Hz (the window's sliders, mix mode reading the reference).
pub const TICK: Duration = Duration::from_millis(50);
/// Idle, so a laptop's CPU can stay asleep: 4 wakeups a second. Still far inside the APO's
/// `OWNER_TIMEOUT` (3 s) and the 1-3 s windows the beats and the DSP watch are judged over.
pub const IDLE_TICK: Duration = Duration::from_millis(250);
/// A grid point closer than this counts as reached (a wait that woke a hair early): the wait goes
/// on to the next one, never ~0 ms.
const GRID_MIN_WAIT: Duration = Duration::from_millis(1);
/// How much earlier than its interval a check on the grid may fire (`due`). Covers the wake latency
/// and the work before the check; below the busy ticks, so a busy loop fires at most one tick early.
const GRID_SLACK: Duration = Duration::from_millis(20);

/// Where the wake grid starts (the first `grid_wait` of the process).
static GRID_ORIGIN: OnceLock<Instant> = OnceLock::new();

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
/// Busy waits are exact, so a loop that turns busy reacts at once; the idle wait ends on the grid.
pub fn tick_wait(mix_fast: bool, busy: Option<Busy>) -> Duration {
  if mix_fast { MIX_TICK } else { busy.map_or_else(|| grid_wait(IDLE_TICK), |(_, tick)| tick) }
}

/// The wait until the next multiple of `period` on the process's wake grid. Every idle wait of
/// the service's threads ends there, so their work runs in one burst and the CPU then sleeps a
/// whole `IDLE_TICK` instead of waking for each thread on its own (Windows does not coalesce
/// high-resolution timers). `period` is a multiple of `IDLE_TICK`; the first wait after a busy
/// stretch is shorter, and none is longer than `period` + `GRID_MIN_WAIT`.
pub fn grid_wait(period: Duration) -> Duration {
  until_grid(GRID_ORIGIN.get_or_init(Instant::now).elapsed(), period)
}

/// `since`: when the work last ran, None = never or wanted now. Due when `interval` has passed,
/// give or take `GRID_SLACK`: on the grid the next pass comes exactly one period later, and a
/// plain `>= interval` would miss it about half the time and run a period late.
pub fn due(since: Option<Duration>, interval: Duration) -> bool {
  since.is_none_or(|s| s + GRID_SLACK >= interval)
}

/// `thread::sleep`; with `cpu-stats` the wakeup is counted (`[Cpu] wakeups`).
pub fn sleep(wait: Duration) {
  std::thread::sleep(wait);
  #[cfg(feature = "cpu-stats")]
  crate::cpu_stats::woke();
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

/// `grid_wait` at `since_origin` after the grid's origin.
fn until_grid(since_origin: Duration, period: Duration) -> Duration {
  let period_ns = period.as_nanos().max(1);
  let left = period_ns - since_origin.as_nanos() % period_ns;
  let left = Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX));
  if left < GRID_MIN_WAIT { left + period } else { left }
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
    let idle = tick_wait(false, None);
    assert!(idle >= GRID_MIN_WAIT && idle <= IDLE_TICK + GRID_MIN_WAIT, "idle waits for the grid: {idle:?}");
  }

  #[test]
  fn grid_waits_land_on_multiples_and_are_never_about_zero_or_much_longer() {
    let ms = Duration::from_millis;
    for at in [0, 1, 3, 100, 249, 250, 251, 499, 500, 1234, 999_999] {
      for period in [IDLE_TICK, 2 * IDLE_TICK, 8 * IDLE_TICK] {
        let wait = until_grid(ms(at), period);
        assert!(wait >= GRID_MIN_WAIT && wait <= period + GRID_MIN_WAIT, "{at} ms, {period:?}: {wait:?}");
        assert_eq!((ms(at) + wait).as_nanos() % period.as_nanos(), 0, "{at} ms, {period:?} ends on a multiple");
      }
    }
    assert_eq!(until_grid(ms(0), IDLE_TICK), IDLE_TICK, "on a grid point: a whole period");
    assert_eq!(until_grid(ms(1100), IDLE_TICK), ms(150));
    assert_eq!(until_grid(ms(1100), 2 * IDLE_TICK), ms(400), "the router's 500 ms: every second grid point");
    assert_eq!(until_grid(ms(1100), 8 * IDLE_TICK), ms(900));
    // Woke a hair before the grid point: the next one, not a second pass right away.
    assert_eq!(until_grid(Duration::from_micros(249_700), IDLE_TICK), Duration::from_micros(250_300));
  }

  #[test]
  fn after_a_busy_stretch_the_first_idle_wait_snaps_back_to_the_grid() {
    let ms = Duration::from_millis;
    // Busy 50 ms ticks from 1000 ms on, ending at 1310 ms (work included): one short wait, then whole periods.
    let first = until_grid(ms(1310), IDLE_TICK);
    assert_eq!(first, ms(190));
    assert_eq!(until_grid(ms(1310) + first + ms(2), IDLE_TICK), ms(248), "2 ms of work, then the next point");
  }

  #[test]
  fn a_check_on_the_grid_fires_on_the_pass_one_period_later() {
    let ms = Duration::from_millis;
    assert!(due(None, ms(250)), "never ran, or wanted now");
    assert!(due(Some(ms(249)), ms(250)), "a pass that woke a hair earlier than the last one");
    assert!(due(Some(ms(1000)), ms(1000)));
    assert!(!due(Some(ms(200)), ms(250)), "a busy 50 ms tick before: not yet");
    assert!(!due(Some(ms(750)), ms(1000)), "three idle passes: not yet");
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
