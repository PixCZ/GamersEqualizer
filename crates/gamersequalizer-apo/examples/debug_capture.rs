//! Records the mic chain inside the APO: every stage as a WAV plus one row of variables per
//! block. Needs an APO built with the `debug-capture` feature
//! (`scripts/dev-rebuild.ps1 -Apo -DebugCapture`, then install it) and a running mic stream.
//!
//!   cargo run --release -p gamersequalizer-apo --example debug_capture -- [seconds] [out_dir]
//!
//! Default: 60 s into `test_data/private/captures/<UTC time>/` (git-ignored).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gamersequalizer_common::debug_capture::*;
use gamersequalizer_mic_vst::debug_capture::DebugCaptureMap;

const START_TIMEOUT: Duration = Duration::from_secs(5);
/// No new frames for this long: the stream stopped, save what there is.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);

fn main() {
  let mut args = std::env::args().skip(1);
  let seconds: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEBUG_CAPTURE_MAX_SECONDS).clamp(1, DEBUG_CAPTURE_MAX_SECONDS);
  let out_dir = args.next().map(PathBuf::from).unwrap_or_else(|| {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/private/captures").join(utc_stamp())
  });
  if let Err(e) = run(seconds, &out_dir) {
    eprintln!("debug capture failed: {e}");
    std::process::exit(1);
  }
}

fn run(seconds: u32, out_dir: &Path) -> Result<(), String> {
  let map = DebugCaptureMap::open_existing().map_err(|e| {
    format!("{e}\n  The installed APO has no debug capture (build it with `dev-rebuild.ps1 -Apo -DebugCapture` and install),\n  or no mic stream started since it was installed (open something that uses the mic).")
  })?;
  let h = map.header();
  let target = (seconds * DEBUG_CAPTURE_RATE) as usize;
  h.target_frames.store(target as u32, Ordering::Relaxed);
  h.frames.store(0, Ordering::Relaxed);
  h.state.store(STATE_REQUESTED, Ordering::Release);
  println!("requested {seconds} s, waiting for the mic stream...");

  let start = Instant::now();
  while h.state.load(Ordering::Acquire) == STATE_REQUESTED {
    if start.elapsed() > START_TIMEOUT {
      h.state.store(STATE_IDLE, Ordering::Release);
      return Err("no mic stream picked up the request in 5 s (is an app using the microphone?)".into());
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  println!("recording (audiodg pid {})", h.writer_pid.load(Ordering::Relaxed));

  let (mut last_frames, mut last_change, mut last_print) = (0u32, Instant::now(), Instant::now());
  while h.state.load(Ordering::Acquire) == STATE_RECORDING {
    let frames = h.frames.load(Ordering::Acquire);
    if frames != last_frames {
      (last_frames, last_change) = (frames, Instant::now());
    } else if last_change.elapsed() > STALL_TIMEOUT {
      println!("stream stopped, saving what was recorded");
      h.state.store(STATE_IDLE, Ordering::Release);
      std::thread::sleep(Duration::from_millis(100)); // let a block in flight finish
      break;
    }
    if last_print.elapsed() >= Duration::from_secs(1) {
      print!("\r{:5.1} / {seconds} s", frames as f32 / DEBUG_CAPTURE_RATE as f32);
      let _ = std::io::stdout().flush();
      last_print = Instant::now();
    }
    std::thread::sleep(Duration::from_millis(50));
  }
  println!();

  let frames = h.frames.load(Ordering::Acquire) as usize;
  let blocks = h.blocks.load(Ordering::Acquire) as usize;
  std::fs::create_dir_all(out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
  for (i, (name, _)) in DEBUG_STAGES.iter().enumerate() {
    write_wav(&out_dir.join(format!("{name}.wav")), map.stage(i, frames)).map_err(|e| format!("{name}.wav: {e}"))?;
  }
  write_vars(&out_dir.join("vars.tsv"), map.vars(blocks)).map_err(|e| format!("vars.tsv: {e}"))?;
  write_info(&out_dir.join("info.txt"), &map, frames, blocks).map_err(|e| format!("info.txt: {e}"))?;
  h.state.store(STATE_IDLE, Ordering::Release);
  println!("{:.1} s, {blocks} blocks -> {}", frames as f32 / DEBUG_CAPTURE_RATE as f32, out_dir.display());
  Ok(())
}

/// Mono 32-bit float WAV at the capture rate.
fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<()> {
  let data_len = (samples.len() * 4) as u32;
  let mut out = Vec::with_capacity(44 + samples.len() * 4);
  out.extend_from_slice(b"RIFF");
  out.extend_from_slice(&(36 + data_len).to_le_bytes());
  out.extend_from_slice(b"WAVEfmt ");
  out.extend_from_slice(&16u32.to_le_bytes());
  out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
  out.extend_from_slice(&1u16.to_le_bytes()); // mono
  out.extend_from_slice(&DEBUG_CAPTURE_RATE.to_le_bytes());
  out.extend_from_slice(&(DEBUG_CAPTURE_RATE * 4).to_le_bytes());
  out.extend_from_slice(&4u16.to_le_bytes());
  out.extend_from_slice(&32u16.to_le_bytes());
  out.extend_from_slice(b"data");
  out.extend_from_slice(&data_len.to_le_bytes());
  samples.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
  std::fs::write(path, out)
}

fn write_vars(path: &Path, rows: &[[f32; DEBUG_VARS.len()]]) -> std::io::Result<()> {
  let mut out = String::new();
  out.push_str(&DEBUG_VARS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join("\t"));
  out.push('\n');
  for row in rows {
    out.push_str(&row.iter().map(|v| format!("{v}")).collect::<Vec<_>>().join("\t"));
    out.push('\n');
  }
  std::fs::write(path, out)
}

fn write_info(path: &Path, map: &DebugCaptureMap, frames: usize, blocks: usize) -> std::io::Result<()> {
  let h = map.header();
  let r = Ordering::Relaxed;
  let mut s = format!(
    "captured (UTC): {}\nseconds: {:.3}\nframes: {frames}\nblocks: {blocks}\nchain rate: {DEBUG_CAPTURE_RATE} Hz (mono f32)\n\
     device rate: {} Hz\naudiodg pid: {}\naec pre-delay: {} samples\ndelay search range: 0..{} samples\n\
     start qpc (100 ns): {}\n\nNote: load includes the capture's own writes (debug build).\n\nfiles:\n",
    utc_stamp(),
    frames as f32 / DEBUG_CAPTURE_RATE as f32,
    h.device_rate.load(r),
    h.writer_pid.load(r),
    h.aec_pre_delay_samples.load(r),
    h.corr_max_delay_samples.load(r),
    h.start_qpc_hns.load(r),
  );
  for (name, desc) in DEBUG_STAGES {
    s.push_str(&format!("  {name}.wav  {desc}\n"));
  }
  s.push_str("\nvars.tsv columns (one row per block):\n");
  for (name, desc) in DEBUG_VARS {
    s.push_str(&format!("  {name:<22} {desc}\n"));
  }
  std::fs::write(path, s)
}

/// `YYYYMMDD-HHMMSS` in UTC (std has no local time zone).
fn utc_stamp() -> String {
  let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
  let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
  // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z - era * 146_097;
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = doy - (153 * mp + 2) / 5 + 1;
  let month = if mp < 10 { mp + 3 } else { mp - 9 };
  let year = yoe + era * 400 + i64::from(month <= 2);
  format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}
