//! Checks whether per-stream (SFX) APO instances can be paired with apps: every round it
//! samples each audio session's peak meter on the default output every 10 ms, then checks
//! which session's meter each stream-probe instance's envelope reproduces (`hold_match`).
//! `STREAM_PROBE_CSV=<dir>` also writes the raw series of each round.
//!
//!   cargo run -p gamersequalizer-apo --example stream_probe -- 10
//!
//! Needs the stream EQ installed on the default output
//! (`scripts/apo-install.ps1 -Flow Render -Slot SFX`) and the app playing something.

use std::time::Duration;

use gamersequalizer_common::shm::APO_SHM_NAME;
use gamersequalizer_common::streams::{
  ENVELOPE_BLOCK_HNS, ENVELOPE_LEN, HOLD_PAD_AFTER, HOLD_PAD_BEFORE, HoldMatch, StreamSlot, hold_match,
};
use gamersequalizer_mic_chain::SharedMemoryClient;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
  IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
  OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{GUID, Interface, PWSTR};

/// One round of sampling; must stay below the APO's 2.56 s envelope history.
const ROUND_BLOCKS: usize = 200;
/// The padded envelope read after the round (a few blocks late) must still be in the history.
const _: () = assert!(ROUND_BLOCKS + HOLD_PAD_BEFORE + HOLD_PAD_AFTER + 20 < ENVELOPE_LEN);
/// Match share needed to call a pairing, and the lead over the runner-up. Below 1 on
/// purpose: a session with several streams matches each only where it was the loudest.
const MIN_SCORE: f32 = 0.5;
const MIN_MARGIN: f32 = 0.3;

const KNOWN_MODES: [(u128, &str); 3] = [
  (0xc18e2f7e_933d_4965_b7d1_1eef228d2af3, "Default"),
  (0x9e90ea20_b493_4fd1_a1a8_7e1361a956cf, "Raw"),
  (0x98951333_b9cd_48b1_a0a3_ff40682d73f7, "Communications"),
];

struct Session {
  pid: u32,
  exe: String,
  meter: IAudioMeterInformation,
  peaks: Vec<f32>,
}

fn main() {
  let rounds: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
  let shm = match SharedMemoryClient::open_existing(APO_SHM_NAME) {
    Ok(s) => s,
    Err(e) => {
      println!("APO mapping not available ({e}). Is the APO installed and something playing?");
      return;
    }
  };
  // SAFETY: COM init for this thread; the rest are COM calls on interfaces we own.
  let manager = unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let devices: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      Ok(d) => d,
      Err(e) => return println!("MMDeviceEnumerator: {e}"),
    };
    let manager = devices
      .GetDefaultAudioEndpoint(eRender, eConsole)
      .and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None));
    match manager {
      Ok(m) => m,
      Err(e) => return println!("session manager of the default output: {e}"),
    }
  };

  for round in 1..=rounds {
    let mut sessions = list_sessions(&manager);
    let Some(first_block) = now_hns().map(|t| t / ENVELOPE_BLOCK_HNS) else {
      return println!("QueryPerformanceCounter failed");
    };
    for s in &mut sessions {
      s.peaks = vec![0.0; ROUND_BLOCKS];
    }
    // Sample every 10 ms, keyed by QPC block so sleep jitter only drops/duplicates points.
    let offset_now = || {
      let block = now_hns()? / ENVELOPE_BLOCK_HNS;
      block.checked_sub(first_block).map(|o| o as usize).filter(|&o| o < ROUND_BLOCKS)
    };
    while let Some(offset) = offset_now() {
      for s in &mut sessions {
        // SAFETY: COM call on an interface we own.
        let peak = unsafe { s.meter.GetPeakValue() }.unwrap_or(0.0);
        if let Some(p) = s.peaks.get_mut(offset) {
          *p = p.max(peak);
        }
      }
      std::thread::sleep(Duration::from_millis(5));
    }
    report(round, &shm, &sessions, first_block);
  }
}

fn report(round: u32, shm: &SharedMemoryClient, sessions: &[Session], first_block: u64) {
  let table = &shm.layout().streams;
  let owned: Vec<(usize, &StreamSlot, u64)> = table
    .slots
    .iter()
    .enumerate()
    .map(|(i, s)| (i, s, s.owner.load(Ordering::Acquire)))
    .filter(|(_, _, owner)| *owner != 0)
    .collect();
  // The newest instance belongs to the running audiodg; other pids are stale slots.
  let audiodg = owned.iter().max_by_key(|(_, _, o)| *o as u32).map(|(_, _, o)| (*o >> 32) as u32);
  let now = now_hns().unwrap_or(0);
  println!("\n=== round {round}: {} stream instance(s), {} session(s) ===", owned.len(), sessions.len());
  for s in sessions {
    let loud = s.peaks.iter().fold(0.0f32, |m, p| m.max(*p));
    println!("  session pid {:>6} {:<28} peak {:.3}", s.pid, s.exe, loud);
  }
  if let Some(dir) = std::env::var_os("STREAM_PROBE_CSV") {
    write_csv(std::path::Path::new(&dir).join(format!("round_{round}.csv")), &owned, sessions, first_block);
  }
  let mut envelope = vec![0.0f32; ROUND_BLOCKS + HOLD_PAD_BEFORE + HOLD_PAD_AFTER];
  for (index, slot, owner) in owned {
    let stale = Some((owner >> 32) as u32) != audiodg;
    slot.envelope(first_block.saturating_sub(HOLD_PAD_BEFORE as u64), &mut envelope);
    let loud = envelope.iter().fold(0.0f32, |m, p| m.max(*p));
    let age_ms = |t: u64| if t == 0 { -1.0 } else { now.saturating_sub(t) as f64 / 10_000.0 };
    println!(
      "  slot {index:>2} #{:<5} {}Hz x{} mode {:<16} age {:>8.0} ms, last audio {:>7.0} ms ago, frames {:>9}, peak {loud:.3}{}",
      owner as u32,
      slot.sample_rate.load(Ordering::Relaxed),
      slot.channels.load(Ordering::Relaxed),
      mode_name(slot.mode()),
      age_ms(slot.created_hns.load(Ordering::Relaxed)),
      age_ms(slot.last_audio_hns.load(Ordering::Relaxed)),
      slot.frames.load(Ordering::Relaxed),
      if stale { "  (stale: old audiodg)" } else { "" },
    );
    if loud == 0.0 {
      println!("      silent this round: nothing to pair");
      continue;
    }
    let mut scores: Vec<(HoldMatch, &Session)> =
      sessions.iter().filter_map(|s| hold_match(&envelope, &s.peaks).map(|m| (m, s))).collect();
    scores.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
    let runner_up = scores.get(1).map_or(0.0, |s| s.0.score);
    let ms = (ENVELOPE_BLOCK_HNS / 10_000) as isize;
    match scores.first() {
      Some((m, s)) => {
        let verdict = if m.score >= MIN_SCORE && m.score - runner_up >= MIN_MARGIN { "PAIRED" } else { "unsure" };
        println!(
          "      -> {verdict}: {} (pid {}) {:.0} % of {} points, hold {} ms, lag {} ms, gain {:.3}; runner-up {:.0} %",
          s.exe,
          s.pid,
          m.score * 100.0,
          m.points,
          m.window as isize * ms,
          m.lag * ms,
          m.gain,
          runner_up * 100.0
        );
      }
      None => println!("      -> no session with enough audio to compare"),
    }
  }
}

/// Raw series for offline analysis (`STREAM_PROBE_CSV=<dir>`): one row per 10 ms block.
fn write_csv(path: std::path::PathBuf, owned: &[(usize, &StreamSlot, u64)], sessions: &[Session], first_block: u64) {
  let mut columns: Vec<(String, Vec<f32>)> = owned
    .iter()
    .map(|(index, slot, _)| {
      let mut env = vec![0.0f32; ROUND_BLOCKS];
      slot.envelope(first_block, &mut env);
      (format!("slot{index}"), env)
    })
    .collect();
  columns.extend(sessions.iter().map(|s| (format!("{}_{}", s.exe, s.pid), s.peaks.clone())));
  let mut text =
    std::iter::once("block".to_string()).chain(columns.iter().map(|c| c.0.clone())).collect::<Vec<_>>().join(",");
  for row in 0..ROUND_BLOCKS {
    text.push('\n');
    text.push_str(&row.to_string());
    for (_, values) in &columns {
      text.push_str(&format!(",{}", values.get(row).copied().unwrap_or(0.0)));
    }
  }
  if let Err(e) = std::fs::write(&path, text) {
    println!("  csv {}: {e}", path.display());
  }
}

fn list_sessions(manager: &IAudioSessionManager2) -> Vec<Session> {
  let mut out = Vec::new();
  // SAFETY: COM calls on interfaces we own.
  unsafe {
    let Ok(list) = manager.GetSessionEnumerator() else { return out };
    for i in 0..list.GetCount().unwrap_or(0) {
      let Ok(control) = list.GetSession(i) else { continue };
      let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
      let Ok(meter) = control.cast::<IAudioMeterInformation>() else { continue };
      let pid = control2.GetProcessId().unwrap_or(0);
      let exe = if pid == 0 { "(system sounds)".to_string() } else { exe_name(pid).unwrap_or_else(|| "?".into()) };
      out.push(Session { pid, exe, meter, peaks: Vec::new() });
    }
  }
  out
}

fn exe_name(pid: u32) -> Option<String> {
  // SAFETY: handle closed below; buffer outlives the call.
  unsafe {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
    let _ = CloseHandle(process);
    ok.ok()?;
    let path = String::from_utf16_lossy(buf.get(..len as usize)?);
    path.rsplit('\\').next().map(str::to_string)
  }
}

fn mode_name(mode: u128) -> String {
  if mode == 0 {
    return "?".into();
  }
  KNOWN_MODES
    .iter()
    .find(|(m, _)| *m == mode)
    .map_or_else(|| format!("{:?}", GUID::from_u128(mode)), |(_, n)| n.to_string())
}

fn now_hns() -> Option<u64> {
  let (mut counter, mut freq) = (0i64, 0i64);
  // SAFETY: out parameters are locals.
  unsafe {
    QueryPerformanceCounter(&mut counter).ok()?;
    QueryPerformanceFrequency(&mut freq).ok()?;
  }
  u64::try_from(counter as i128 * 10_000_000 / i128::from(freq.max(1))).ok()
}
