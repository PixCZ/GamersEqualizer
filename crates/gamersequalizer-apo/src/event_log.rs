//! Notes from inside audiodg to the Windows Application log (source `GamersEqualizer`): each
//! step Windows takes towards a running effect (class, instance, init, formats, stream) and why
//! it passes audio through unchanged. audiodg has no logger, and from outside an APO that
//! silently passes audio through looks exactly like one Windows never loaded. The service
//! copies them into the app log (`[ApoNote]`, `gamersequalizer-daemon/src/apo_notes.rs`); each
//! names the process and the DLL build, which proves which build audiodg loaded.
//!
//! Each distinct note once per process run (a mic opened a hundred times adds one entry), at
//! most `MAX_NOTES`. Never call this from the real-time path: it locks and makes system calls.
//! The source is not registered, so Event Viewer adds "description not found" before the text;
//! the text itself is the event's first string.

use std::sync::Mutex;
use windows::Win32::System::EventLog::{
  DeregisterEventSource, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Bound per process run, so a long session cannot grow the list (or the log) forever. Roomy:
/// multi-device installs and render format queries must not use up the budget before streams start.
const MAX_NOTES: usize = 512;
const EVENT_ID: u32 = 1;

static WRITTEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub enum Level {
  Info,
  Warning,
}

/// Writes `text` unless a note with the same `key` was written in this process. Best effort:
/// a blocked or full log changes nothing for the audio.
pub fn note(level: Level, key: &str, text: &str) {
  let first = WRITTEN.lock().is_ok_and(|mut written| first_time(&mut written, key));
  // Unit tests build the chain too: they must not write into the developer's event log.
  if !first || cfg!(test) {
    return;
  }
  // Called from COM entry points of audiodg: nothing may unwind out of here.
  let _ = std::panic::catch_unwind(|| write(level, text));
}

fn write(level: Level, text: &str) {
  let kind = match level {
    Level::Info => EVENTLOG_INFORMATION_TYPE,
    Level::Warning => EVENTLOG_WARNING_TYPE,
  };
  // Normally audiodg.exe; Sound settings also create effects, only to list them.
  let process = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
  let process = process.unwrap_or_else(|| "?".to_string());
  let build = env!("GAMERSEQUALIZER_VERSION");
  let text = HSTRING::from(format!("{process} pid {}, build {build}: {text}", std::process::id()));
  // SAFETY: NUL-terminated strings that outlive the calls; the handle is closed below.
  unsafe {
    let Ok(source) = RegisterEventSourceW(PCWSTR::null(), w!("GamersEqualizer")) else {
      return;
    };
    let strings = [PCWSTR(text.as_ptr())];
    let _ = ReportEventW(source, kind, 0, EVENT_ID, None, 0, Some(&strings), None);
    let _ = DeregisterEventSource(source);
  }
}

/// True (and remembered) the first time `key` comes; false for repeats and once full.
fn first_time(written: &mut Vec<String>, key: &str) -> bool {
  if written.len() >= MAX_NOTES || written.iter().any(|k| k == key) {
    return false;
  }
  written.push(key.to_string());
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_key_is_written_once_and_the_list_is_bounded() {
    let mut written = Vec::new();
    assert!(first_time(&mut written, "mic-stop:processed"));
    assert!(!first_time(&mut written, "mic-stop:processed"));
    assert!(first_time(&mut written, "mic-stop:no owner"));
    for i in 0..(2 * MAX_NOTES) {
      first_time(&mut written, &format!("k{i}"));
    }
    assert_eq!(written.len(), MAX_NOTES);
    assert!(!first_time(&mut written, "new after full"));
  }
}
