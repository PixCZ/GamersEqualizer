//! Feature `cpu-stats` (`scripts/dev-rebuild.ps1 -CpuStats`): once a minute the debug log says
//! where the CPU time went. Each process (service, window) writes one line with the time of
//! its threads; the service adds one with the time our effects took inside audiodg, which an
//! APO built with the same feature counts (`gamersequalizer_mic_chain::cpu_stats`). Debug lines
//! reach the log file only with "Save debug to disk" on. The service loop adds one line with the
//! CPU time of each of its sections (`LoopSections`), and one with its threads' wakeups and how
//! many distinct instants they fell on (`woke`): CPU time alone cannot show whether the CPU could
//! sleep long between them. Never ship a build with it.

use gamersequalizer_mic_chain::cpu_stats::{CpuStatsMap, EffectTime};
use log::{debug, warn};
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HLOCAL, LocalFree};
use windows::Win32::System::Diagnostics::ToolHelp::{
  CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
  GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, GetProcessTimes, GetThreadDescription,
  GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};

const INTERVAL: Duration = Duration::from_secs(60);
/// Threads below this share of a core are left out of the line (the total has them).
const SHOWN_FROM_PERCENT: f64 = 0.005;
/// The effects of `CpuStats`, in the order `Effects::log` reads them.
const EFFECT_NAMES: [&str; 3] = ["mic chain", "output EQ", "stream effects"];
/// Wakeups this close to the first of a run count as one instant (the CPU woke once for them).
const SAME_INSTANT: Duration = Duration::from_millis(2);

/// Every completed wait of a service thread this minute (`woke`): thread name and when.
static WAKES: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());

struct ThreadTime {
  id: u32,
  /// When the thread started (100 ns): Windows gives the id of an ended thread to a new one.
  created: u64,
  /// Empty when the thread has none.
  name: String,
  /// Kernel + user time since the thread started, in 100 ns.
  cpu: u64,
}

/// The counters of one effect as last read.
#[derive(Clone, Copy, Default)]
struct EffectRead {
  busy_ns: u64,
  blocks: u64,
  longest_ns: u64,
}

/// The service's view of the time the effects count inside audiodg.
#[derive(Default)]
struct Effects {
  map: Option<CpuStatsMap>,
  seen: [EffectRead; 3],
  /// Why there were no numbers last time; logged when it changes.
  error: String,
}

/// CPU of each named section of the service loop (`timed!` in `daemon.rs`), on the loop's thread.
/// Cycles are exact for short sections; they are turned into time with the thread's own CPU time
/// over the minute, so the line agrees with the `main` share of the `[Cpu] service` line.
pub struct LoopSections {
  since: Instant,
  /// This thread's cycles and CPU time (100 ns) when the minute began.
  start: (u64, u64),
  /// Cycles and calls per section.
  sections: BTreeMap<&'static str, (u64, u32)>,
}

// The `windows` crate has it behind a feature (`Win32_System_WindowsProgramming`) nothing else needs.
#[link(name = "kernel32")]
unsafe extern "system" {
  fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
}

/// Call once from the main thread, after the logger is up. `service`: this process is the
/// service, which also reports the effects.
pub fn start(service: bool) {
  let process = if service { "service" } else { "window" };
  // SAFETY: no arguments, cannot fail.
  let main_thread = unsafe { GetCurrentThreadId() };
  let spawned = thread::Builder::new().name("cpu-stats".into()).spawn(move || {
    let mut threads = thread_times(main_thread);
    let mut process_cpu = process_time();
    let mut effects = Effects::default();
    let mut at = Instant::now();
    loop {
      thread::sleep(INTERVAL);
      let elapsed = at.elapsed();
      at = Instant::now();
      let (now, cpu_now) = (thread_times(main_thread), process_time());
      let line = threads_line(&threads, &now, cpu_now.saturating_sub(process_cpu), elapsed);
      debug!("[Cpu] {process}, {:.0} s: {line}", elapsed.as_secs_f64());
      (threads, process_cpu) = (now, cpu_now);
      let mut wakes = WAKES.lock().map(|mut w| std::mem::take(&mut *w)).unwrap_or_default();
      if service {
        debug!("[Cpu] wakeups, {:.0} s: {}", elapsed.as_secs_f64(), wakeups_line(&mut wakes, elapsed));
        effects.log(elapsed);
      }
    }
  });
  if let Err(e) = spawned {
    warn!("[Cpu] no CPU statistics: their thread did not start ({e})");
  }
}

/// The calling thread's wait just ended (`worker::sleep`, the service loop's idle wait).
pub fn woke() {
  let name = thread::current().name().unwrap_or("unnamed").to_string();
  if let Ok(mut wakes) = WAKES.lock() {
    wakes.push((name, Instant::now()));
  }
}

impl Effects {
  fn log(&mut self, elapsed: Duration) {
    let opened = self.map.is_none();
    if opened {
      match CpuStatsMap::open_existing() {
        Ok(map) => self.map = Some(map),
        Err(e) => {
          if e != self.error {
            debug!(
              "[Cpu] effects in audiodg: no numbers, an APO built with -CpuStats makes them with its first stream ({e})"
            );
            self.error = e;
          }
          return;
        }
      }
    }
    let Some(map) = &self.map else { return };
    let stats = map.stats();
    let now = [read(&stats.mic), read(&stats.output), read(&stats.stream)];
    // The counters run since audiodg started: the first read only sets where the next minute begins.
    if !opened && let Some(line) = effects_line(&self.seen, &now, elapsed) {
      debug!("[Cpu] effects in audiodg, {:.0} s: {line}", elapsed.as_secs_f64());
    }
    self.seen = now;
  }
}

impl LoopSections {
  pub fn begin() -> Self {
    Self { since: Instant::now(), start: (thread_cycles(), own_thread_time()), sections: BTreeMap::new() }
  }

  /// Where a section begins; hand it to `stop`.
  pub fn start(&self) -> u64 {
    thread_cycles()
  }

  /// Counts the section begun at `started`; once a minute logs the line and starts the next minute.
  pub fn stop(&mut self, name: &'static str, started: u64) {
    let (cycles, calls) = self.sections.entry(name).or_default();
    *cycles += thread_cycles().saturating_sub(started);
    *calls += 1;
    if self.since.elapsed() >= INTERVAL {
      let elapsed = self.since.elapsed();
      let (cycles, ticks) = (thread_cycles(), own_thread_time());
      let line =
        sections_line(&self.sections, cycles.saturating_sub(self.start.0), ticks.saturating_sub(self.start.1), elapsed);
      debug!("[Cpu] service loop sections, {:.0} s: {line}", elapsed.as_secs_f64());
      *self = Self::begin();
    }
  }
}

/// Each section's share of a core and its calls, the busiest first. `thread_cycles` and
/// `thread_ticks` (100 ns): the whole loop thread in `elapsed`, which converts cycles to time.
fn sections_line(
  sections: &BTreeMap<&'static str, (u64, u32)>,
  thread_cycles: u64,
  thread_ticks: u64,
  elapsed: Duration,
) -> String {
  let thread_percent = thread_ticks as f64 / 1e7 / elapsed.as_secs_f64().max(1e-3) * 100.0;
  let percent = |cycles: u64| thread_percent * cycles as f64 / thread_cycles.max(1) as f64;
  let mut sorted: Vec<_> = sections.iter().collect();
  sorted.sort_by_key(|(_, (cycles, _))| Reverse(*cycles));
  let timed: u64 = sorted.iter().map(|(_, (cycles, _))| cycles).sum();
  let mut parts: Vec<String> =
    sorted.iter().map(|(name, (cycles, calls))| format!("{name} {:.3} % ({calls})", percent(*cycles))).collect();
  parts.push(format!("untimed {:.3} %", percent(thread_cycles.saturating_sub(timed))));
  format!("thread {thread_percent:.3} % of a core: {}", parts.join(", "))
}

/// Cycles this thread ran since it started; 0 when Windows does not say.
fn thread_cycles() -> u64 {
  let mut cycles = 0;
  // SAFETY: the pseudo handle of the current thread is always valid; `cycles` is a local the call fills.
  let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
  cycles
}

/// Kernel + user time of this thread since it started, in 100 ns; 0 when Windows does not say.
fn own_thread_time() -> u64 {
  let (mut created, mut exited, mut kernel, mut user) = <(FILETIME, FILETIME, FILETIME, FILETIME)>::default();
  // SAFETY: the pseudo handle of the current thread is always valid; the four locals are filled by the call.
  let read = unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user) };
  if read.is_ok() { ticks(kernel) + ticks(user) } else { 0 }
}

fn read(effect: &EffectTime) -> EffectRead {
  EffectRead {
    busy_ns: effect.busy_ns.load(Ordering::Relaxed),
    blocks: effect.blocks.load(Ordering::Relaxed),
    longest_ns: effect.longest_ns.swap(0, Ordering::Relaxed),
  }
}

/// Share of one core per thread name since `before`, the busiest first. `process_ticks`: the
/// whole process in that time, which also has the threads that ended since.
fn threads_line(before: &[ThreadTime], now: &[ThreadTime], process_ticks: u64, elapsed: Duration) -> String {
  if now.is_empty() {
    return "Windows listed no threads".to_string();
  }
  // A thread that started in between counts from zero.
  let mut by_name: BTreeMap<&str, (u64, u32)> = BTreeMap::new();
  for thread in now {
    let earlier = before.iter().find(|b| b.id == thread.id && b.created == thread.created).map_or(0, |b| b.cpu);
    let (ticks, count) = by_name.entry(&thread.name).or_default();
    *ticks += thread.cpu.saturating_sub(earlier);
    *count += 1;
  }
  let percent = |ticks: u64| ticks as f64 / 1e7 / elapsed.as_secs_f64().max(1e-3) * 100.0;
  let listed: u64 = by_name.values().map(|(ticks, _)| ticks).sum();
  let total = process_ticks.max(listed);
  let mut shown: Vec<_> = by_name.into_iter().filter(|(_, (ticks, _))| percent(*ticks) >= SHOWN_FROM_PERCENT).collect();
  shown.sort_by_key(|(_, (ticks, _))| Reverse(*ticks));
  let mut parts: Vec<String> = shown
    .iter()
    .map(|(name, (ticks, count))| match (name.is_empty(), count) {
      (true, _) => format!("{count} unnamed {:.2} %", percent(*ticks)),
      (false, 1) => format!("{name} {:.2} %", percent(*ticks)),
      (false, _) => format!("{name} x{count} {:.2} %", percent(*ticks)),
    })
    .collect();
  if percent(total - listed) >= SHOWN_FROM_PERCENT {
    parts.push(format!("threads that ended {:.2} %", percent(total - listed)));
  }
  let detail = if parts.is_empty() { String::new() } else { format!(": {}", parts.join(", ")) };
  format!("{:.2} % of a core in {} threads{detail}", percent(total), now.len())
}

/// Wakeups per second, in all and per thread (the most first), and how many distinct instants
/// they fell on (`SAME_INSTANT`): the CPU leaves its sleep once per instant, not per thread.
fn wakeups_line(wakes: &mut [(String, Instant)], elapsed: Duration) -> String {
  wakes.sort_by_key(|(_, at)| *at);
  let mut by_thread: BTreeMap<&str, u32> = BTreeMap::new();
  let (mut instants, mut run_start) = (0u32, None::<Instant>);
  for (name, at) in wakes.iter() {
    *by_thread.entry(name).or_default() += 1;
    if run_start.is_none_or(|start| at.duration_since(start) > SAME_INSTANT) {
      instants += 1;
      run_start = Some(*at);
    }
  }
  let rate = |n: u32| f64::from(n) / elapsed.as_secs_f64().max(1e-3);
  let mut sorted: Vec<_> = by_thread.into_iter().collect();
  sorted.sort_by_key(|(_, n)| Reverse(*n));
  let parts: Vec<String> = sorted.iter().map(|(name, n)| format!("{name} {:.2}/s", rate(*n))).collect();
  format!("{:.2}/s at {:.2} distinct instants/s: {}", rate(wakes.len() as u32), rate(instants), parts.join(", "))
}

/// What each effect that processed a block since `before` cost; None when none did.
fn effects_line(before: &[EffectRead; 3], now: &[EffectRead; 3], elapsed: Duration) -> Option<String> {
  let seconds = elapsed.as_secs_f64().max(1e-3);
  let parts: Vec<String> = EFFECT_NAMES
    .iter()
    .zip(before.iter().zip(now))
    .filter_map(|(name, (b, n))| {
      let blocks = n.blocks.saturating_sub(b.blocks);
      let busy_ms = n.busy_ns.saturating_sub(b.busy_ns) as f64 / 1e6;
      (blocks > 0).then(|| {
        format!(
          "{name} {:.2} % of a core ({blocks} blocks, average {:.3} ms, longest {:.3} ms)",
          busy_ms / 10.0 / seconds,
          busy_ms / blocks as f64,
          n.longest_ns as f64 / 1e6
        )
      })
    })
    .collect();
  (!parts.is_empty()).then(|| parts.join(", "))
}

/// CPU time of every thread of this process; empty when Windows gives no list.
fn thread_times(main_thread: u32) -> Vec<ThreadTime> {
  let mut threads = Vec::new();
  // SAFETY: `entry` is a local that states its own size; the snapshot and each thread handle are
  // valid from the calls that returned them until they are closed here, after their last use.
  unsafe {
    let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
      return threads;
    };
    let process = GetCurrentProcessId();
    let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
    let mut more = Thread32First(snapshot, &mut entry).is_ok();
    while more {
      if entry.th32OwnerProcessID == process
        && let Ok(thread) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
      {
        let (mut created, mut exited, mut kernel, mut user) = <(FILETIME, FILETIME, FILETIME, FILETIME)>::default();
        if GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user).is_ok() {
          // Rust names the threads it starts, but not the one `main` runs on.
          let name = if entry.th32ThreadID == main_thread { "main".to_string() } else { description(thread) };
          let (created, cpu) = (ticks(created), ticks(kernel) + ticks(user));
          threads.push(ThreadTime { id: entry.th32ThreadID, created, name, cpu });
        }
        let _ = CloseHandle(thread);
      }
      more = Thread32Next(snapshot, &mut entry).is_ok();
    }
    let _ = CloseHandle(snapshot);
  }
  threads
}

/// Kernel + user time of this process since it started, in 100 ns; 0 when Windows does not say.
fn process_time() -> u64 {
  let (mut created, mut exited, mut kernel, mut user) = <(FILETIME, FILETIME, FILETIME, FILETIME)>::default();
  // SAFETY: the pseudo handle of this process is always valid; the four locals are filled by the call.
  let read = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
  if read.is_ok() { ticks(kernel) + ticks(user) } else { 0 }
}

/// The thread's name; empty when it has none.
///
/// # Safety
/// `thread` is an open thread handle with the right to query limited information.
unsafe fn description(thread: HANDLE) -> String {
  // SAFETY: the handle is valid (contract above); Windows allocates the NUL-terminated string,
  // which is read and then freed here, once.
  unsafe {
    let Ok(text) = GetThreadDescription(thread) else {
      return String::new();
    };
    let name = text.to_string().unwrap_or_default();
    let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    name
  }
}

fn ticks(time: FILETIME) -> u64 {
  (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(test)]
mod tests {
  use super::*;

  const MINUTE: Duration = Duration::from_secs(60);

  #[test]
  fn threads_are_summed_by_name_and_sorted_by_time() {
    let before = [thread(1, "main", 0), thread(2, "worker", 10_000_000), thread(4, "old", 50_000_000)];
    // 0.6 s, 0.3 s and 0.12 s of CPU in the minute; thread 3 started in it, thread 4 took the id of one that ended.
    let now = [
      thread(2, "worker", 13_000_000),
      thread(3, "", 1_200_000),
      thread(1, "main", 6_000_000),
      started_later(4, "", 0),
    ];
    assert_eq!(
      threads_line(&before, &now, 0, MINUTE),
      "1.70 % of a core in 4 threads: main 1.00 %, worker 0.50 %, 2 unnamed 0.20 %"
    );
    // The process used 0.3 s more than its living threads.
    assert_eq!(
      threads_line(&before, &now, 13_200_000, MINUTE),
      "2.20 % of a core in 4 threads: main 1.00 %, worker 0.50 %, 2 unnamed 0.20 %, threads that ended 0.50 %"
    );
    assert_eq!(threads_line(&before, &before, 0, MINUTE), "0.00 % of a core in 3 threads");
    assert_eq!(threads_line(&before, &[], 0, MINUTE), "Windows listed no threads");
  }

  #[test]
  fn only_effects_that_ran_are_listed() {
    let mic_before = EffectRead { busy_ns: 1_000_000_000, blocks: 100, longest_ns: 0 };
    let before = [mic_before, EffectRead::default(), EffectRead::default()];
    let mut now = before;
    assert_eq!(effects_line(&before, &now, MINUTE), None);
    // 1.2 s in 6000 blocks.
    now[0] = EffectRead { busy_ns: 2_200_000_000, blocks: 6100, longest_ns: 400_000 };
    assert_eq!(
      effects_line(&before, &now, MINUTE).as_deref(),
      Some("mic chain 2.00 % of a core (6000 blocks, average 0.200 ms, longest 0.400 ms)")
    );
  }

  #[test]
  fn sections_share_the_thread_time_by_cycles() {
    let sections = BTreeMap::from([("cheap", (100, 240)), ("sync", (600, 240))]);
    // The thread used 0.6 s (1 %) and 1000 cycles in the minute.
    assert_eq!(
      sections_line(&sections, 1000, 6_000_000, MINUTE),
      "thread 1.000 % of a core: sync 0.600 % (240), cheap 0.100 % (240), untimed 0.300 %"
    );
    assert_eq!(sections_line(&BTreeMap::new(), 0, 0, MINUTE), "thread 0.000 % of a core: untimed 0.000 %");
  }

  #[test]
  fn wakeups_within_two_ms_count_as_one_instant() {
    let t0 = Instant::now();
    let at = |ms: u64, us: u64| t0 + Duration::from_millis(ms) + Duration::from_micros(us);
    let mut wakes = vec![
      ("router".to_string(), at(250, 900)),
      ("main".to_string(), at(0, 0)),
      ("aec".to_string(), at(0, 1500)),
      ("main".to_string(), at(250, 0)),
      ("aec".to_string(), at(253, 0)),
    ];
    // 0 ms and 1.5 ms; 250 and 250.9 ms; 253 ms is more than 2 ms after 250.
    assert_eq!(
      wakeups_line(&mut wakes, Duration::from_secs(1)),
      "5.00/s at 3.00 distinct instants/s: aec 2.00/s, main 2.00/s, router 1.00/s"
    );
    assert_eq!(wakeups_line(&mut [], MINUTE), "0.00/s at 0.00 distinct instants/s: ");
  }

  fn thread(id: u32, name: &str, cpu: u64) -> ThreadTime {
    ThreadTime { id, created: 1, name: name.to_string(), cpu }
  }

  fn started_later(id: u32, name: &str, cpu: u64) -> ThreadTime {
    ThreadTime { created: 2, ..thread(id, name, cpu) }
  }
}
