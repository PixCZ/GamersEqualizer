//! App log: every `log` record goes to stderr (dev builds) and to one file shared by the UI
//! and the background service (`%APPDATA%\GamersEqualizer\logs\gamersequalizer.log`).
//! Release builds have no console, so the file is the only place logs survive; the UI's
//! Log page shows its tail.
//!
//! Both processes append to the same file (each record is one write, so lines don't
//! interleave). Shortcut: it is rotated only at startup (> `MAX_LOG_BYTES` -> `.old`), as a
//! file held open by the other process can't be renamed. Upgrade to per-process files if
//! a single session ever produces more than that.
//!
//! Debug lines are written only while "Save debug to disk" (Log page) is on: a marker file next
//! to the log, so both processes follow it (each checks it every `DEBUG_MARKER_CHECK`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const LOG_FILE: &str = "gamersequalizer.log";
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Our crates up to debug (gated by `DEBUG_TO_DISK`); libraries (wgpu logs every frame at info)
/// only warnings.
const DEFAULT_FILTER: &str = "warn,gamersequalizer=debug,gamersequalizer_mic_vst=debug,gamersequalizer_common=debug";
/// Exists = debug lines are saved.
const DEBUG_MARKER: &str = "save_debug";
const DEBUG_MARKER_CHECK: Duration = Duration::from_secs(2);

static DEBUG_TO_DISK: AtomicBool = AtomicBool::new(false);

/// Severity of a log line, parsed back from the file for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LineLevel {
  Error,
  Warn,
  Info,
  Debug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
  pub level: LineLevel,
  /// From this app ("[Tag] ..."), not from a library (wgpu, eframe, ...).
  pub own: bool,
  pub text: String,
}

/// Writes each formatted record to stderr and the log file.
struct TeeWriter {
  file: Option<File>,
}

/// env_logger's filter, plus: debug/trace only while `DEBUG_TO_DISK`.
struct GatedLogger {
  inner: env_logger::Logger,
}

/// Install the logger (bootstrap, before anything logs). `process` tags the lines ("ui" /
/// "service"). `RUST_LOG` still overrides `DEFAULT_FILTER`.
pub fn init(process: &'static str) {
  let path = log_path();
  let file = open_log(&path);
  let file_ok = file.is_some();
  let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(DEFAULT_FILTER))
    .format(move |buf, record| {
      let ts = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S%.3f");
      writeln!(buf, "{ts} {:<5} {process:<7} {}", record.level(), record.args())
    })
    .target(env_logger::Target::Pipe(Box::new(TeeWriter { file })))
    .build();
  let max = inner.filter();
  DEBUG_TO_DISK.store(debug_marker().exists(), Ordering::Relaxed);
  if log::set_boxed_logger(Box::new(GatedLogger { inner })).is_ok() {
    log::set_max_level(max);
  }
  if !file_ok {
    log::warn!("[Log] could not open {}, logging to stderr only", path.display());
  }
  // The other process may switch it: follow the marker file.
  let spawned = std::thread::Builder::new().name("log-debug-marker".into()).spawn(|| loop {
    std::thread::sleep(DEBUG_MARKER_CHECK);
    DEBUG_TO_DISK.store(debug_marker().exists(), Ordering::Relaxed);
  });
  if let Err(e) = spawned {
    log::warn!("[Log] could not watch the debug setting ({e}): it applies from the next start");
  }
}

/// Debug lines are being saved ("Save debug to disk" on the Log page).
pub fn debug_to_disk() -> bool {
  DEBUG_TO_DISK.load(Ordering::Relaxed)
}

/// Switch saving debug lines for both processes (the other one follows within ~2 s).
pub fn set_debug_to_disk(on: bool) {
  let path = debug_marker();
  let result = if on { std::fs::write(&path, b"") } else { std::fs::remove_file(&path) };
  match result {
    Ok(()) => DEBUG_TO_DISK.store(on, Ordering::Relaxed),
    Err(e) if !on && e.kind() == io::ErrorKind::NotFound => DEBUG_TO_DISK.store(false, Ordering::Relaxed),
    Err(e) => log::warn!("[Log] could not switch saving debug lines ({}): {e}", path.display()),
  }
  log::info!("[Log] saving debug lines {}", if on { "on" } else { "off" });
}

pub fn log_dir() -> PathBuf {
  let app_data = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
  app_data.join("GamersEqualizer").join("logs")
}

pub fn log_path() -> PathBuf {
  log_dir().join(LOG_FILE)
}

/// Last lines of the log, at most `max_bytes` read from the end (the first, cut line is dropped).
pub fn read_tail(path: &Path, max_bytes: u64) -> io::Result<Vec<LogLine>> {
  let mut file = File::open(path)?;
  let len = file.metadata()?.len();
  let start = len.saturating_sub(max_bytes);
  file.seek(SeekFrom::Start(start))?;
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes)?;
  let text = String::from_utf8_lossy(&bytes);
  let mut lines = text.lines();
  if start > 0 {
    lines.next();
  }
  Ok(lines.filter(|l| !l.is_empty()).map(parse_line).collect())
}

impl Write for TeeWriter {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    // Dev builds only have a console; failures to write it are irrelevant.
    let _ = io::stderr().write_all(buf);
    if let Some(f) = self.file.as_mut() {
      if f.write_all(buf).is_err() {
        self.file = None; // disk gone / full: keep running, stderr only
      }
    }
    Ok(buf.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    if let Some(f) = self.file.as_mut() {
      let _ = f.flush();
    }
    Ok(())
  }
}

impl log::Log for GatedLogger {
  fn enabled(&self, metadata: &log::Metadata) -> bool {
    (metadata.level() <= log::Level::Info || debug_to_disk()) && self.inner.enabled(metadata)
  }

  fn log(&self, record: &log::Record) {
    if self.enabled(record.metadata()) {
      self.inner.log(record);
    }
  }

  fn flush(&self) {
    self.inner.flush();
  }
}

// --- private helpers ---------------------------------------------------------------------

fn debug_marker() -> PathBuf {
  log_dir().join(DEBUG_MARKER)
}

fn open_log(path: &Path) -> Option<File> {
  let dir = path.parent()?;
  std::fs::create_dir_all(dir).ok()?;
  if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
    // Fails while the other process has the file open: then it is rotated next start.
    let _ = std::fs::rename(path, path.with_extension("log.old"));
  }
  OpenOptions::new().create(true).append(true).open(path).ok()
}

/// "2026-11-05 13:21:30.123 WARN  service [X] msg" -> level from the third field.
fn parse_line(line: &str) -> LogLine {
  let mut fields = line.split_whitespace();
  let level = match fields.nth(2) {
    Some("ERROR") => LineLevel::Error,
    Some("WARN") => LineLevel::Warn,
    Some("DEBUG") | Some("TRACE") => LineLevel::Debug,
    _ => LineLevel::Info,
  };
  // Fields after the level: process, then the message.
  let own = fields.nth(1).is_some_and(|m| m.starts_with('['));
  LogLine { level, own, text: line.to_string() }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_read_tail_drops_cut_line_and_parses_levels() {
    let dir = std::env::temp_dir().join(format!("ge_log_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("t.log");
    let text = "2026-11-05 13:00:00.000 INFO  ui      first line here\n\
                2026-11-05 13:00:01.000 WARN  service [Mic] slow\n\
                2026-11-05 13:00:02.000 ERROR ui      [X] broken\n";
    std::fs::write(&path, text).ok();

    let all = read_tail(&path, 1 << 20).unwrap_or_default();
    assert_eq!(all.iter().map(|l| l.level).collect::<Vec<_>>(), [LineLevel::Info, LineLevel::Warn, LineLevel::Error]);
    assert_eq!(all.iter().map(|l| l.own).collect::<Vec<_>>(), [false, true, true]);

    // Starting mid-file: the partial first line must not show up.
    let tail = read_tail(&path, 80).unwrap_or_default();
    assert_eq!(tail.len(), 1);
    assert!(tail.iter().all(|l| l.text.contains("broken")));
    std::fs::remove_dir_all(&dir).ok();
  }
}
