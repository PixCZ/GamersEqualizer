//! Notes our APO writes from inside audiodg into the Windows Application log (source
//! `GamersEqualizer`, `gamersequalizer-apo/src/event_log.rs`): which stream it started on and
//! why it passes audio through unchanged. Plus what Windows itself recorded about our files:
//! audiodg crashes, Defender detections, Code Integrity blocks. The service copies new ones
//! into its own log and hands the newest to the window, so the log alone tells what happened.

use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::System::EventLog::{
  EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender, EvtRenderEventXml, EVT_HANDLE,
};

/// Newest notes read per check (every 30 s); a stream start writes several, so keep it roomy.
const MAX_NOTES: usize = 100;
/// Newest events of the other logs scanned per check (filtered for our files afterwards).
const MAX_WINDOWS_EVENTS: usize = 100;
/// How long EvtNext may wait for the (local) log, ms.
const NEXT_TIMEOUT_MS: u32 = 1000;
/// `ERROR_NO_MORE_ITEMS`: the query matched nothing (not a failure).
const NO_MORE_ITEMS: u32 = 259;
/// A log that stays unreadable: `ERROR_ACCESS_DENIED` (needs admin), `ERROR_EVT_CHANNEL_NOT_FOUND`
/// (e.g. Defender replaced by another antivirus). Other errors are taken as passing.
const ACCESS_DENIED: u32 = 5;
const CHANNEL_NOT_FOUND: u32 = 15007;

/// One note: when (UTC, as Windows writes it, `2026-11-10T19:29:41.12Z`) and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApoNote {
  pub time: String,
  pub text: String,
}

/// Newest first (empty: there are none). None when the log could not be read (a timeout, no
/// access): the caller keeps what it had, so old notes are not logged again later.
pub fn newest() -> Option<Vec<ApoNote>> {
  let events = query(w!("Application"), w!("*[System[Provider[@Name='GamersEqualizer']]]"), MAX_NOTES).ok()?;
  Some(events.iter().filter_map(|xml| parse(xml)).collect())
}

/// What Windows recorded about our effects, as notes ("Windows: ..."): audiodg crashes (any
/// module: a crash elsewhere also restarts our effects), Defender detections and Code Integrity
/// blocks naming our files. A log that stays unreadable gives one note saying so (Defender and
/// Code Integrity may need administrator rights); a passing error gives None, like `newest`.
pub fn windows_events() -> Option<Vec<ApoNote>> {
  let crashes =
    query(w!("Application"), w!("*[System[Provider[@Name='Application Error'] and EventID=1000]]"), MAX_WINDOWS_EVENTS);
  let defender = query(
    w!("Microsoft-Windows-Windows Defender/Operational"),
    w!("*[System[EventID=1116 or EventID=1117]]"),
    MAX_WINDOWS_EVENTS,
  );
  // Blocks and audit-mode "would block" of a file; the busy 3089 detail events stay out.
  let integrity = query(
    w!("Microsoft-Windows-CodeIntegrity/Operational"),
    w!("*[System[EventID=3033 or EventID=3034 or EventID=3063 or EventID=3064 or EventID=3076 or EventID=3077]]"),
    MAX_WINDOWS_EVENTS,
  );
  let mut notes = Vec::new();
  for (what, result, about) in [
    ("audiodg crashes", crashes, "audiodg.exe"),
    ("Defender detections", defender, "gamersequalizer"),
    ("Code Integrity events", integrity, "gamersequalizer"),
  ] {
    match result {
      Ok(events) => {
        notes.extend(events.iter().filter(|xml| xml.to_ascii_lowercase().contains(about)).map(|xml| windows_note(xml)))
      }
      Err(e) if [ACCESS_DENIED, CHANNEL_NOT_FOUND].iter().any(|&code| e.code() == HRESULT::from_win32(code)) => {
        notes.push(ApoNote { time: String::new(), text: format!("Windows: {what} not readable ({e})") })
      }
      Err(_) => return None,
    }
  }
  Some(notes)
}

/// XML of up to `max` newest events of `channel` matching `xpath` (empty when none match).
fn query(channel: PCWSTR, xpath: PCWSTR, max: usize) -> windows::core::Result<Vec<String>> {
  let mut events = Vec::new();
  // SAFETY: every handle these calls return is closed below; buffers are owned here.
  unsafe {
    let flags = EvtQueryChannelPath.0 | EvtQueryReverseDirection.0;
    let query = EvtQuery(None, channel, xpath, flags)?;
    let mut raw = vec![0isize; max];
    let mut returned = 0u32;
    let read = match EvtNext(query, &mut raw, NEXT_TIMEOUT_MS, 0, &mut returned) {
      Ok(()) => Ok(()),
      Err(e) if e.code() == HRESULT::from_win32(NO_MORE_ITEMS) => Ok(()),
      Err(e) => Err(e),
    };
    if read.is_ok() {
      for &handle in raw.iter().take(returned as usize) {
        let event = EVT_HANDLE(handle);
        events.extend(render_xml(event));
        let _ = EvtClose(event);
      }
    }
    let _ = EvtClose(query);
    read.map(|()| events)
  }
}

/// One line for an audiodg crash, a Defender detection or a Code Integrity event.
fn windows_note(xml: &str) -> ApoNote {
  let time = between(xml, "SystemTime='", "'").unwrap_or_default().to_string();
  let field = |name: &str| between(xml, &format!("Name='{name}'>"), "</Data>").map(unescape);
  let id = between(xml, "<EventID", "</EventID>").and_then(|s| s.rsplit('>').next()).unwrap_or("?");
  let provider = between(xml, "Provider Name='", "'").unwrap_or_default();
  let text = if provider == "Application Error" {
    let module = field("ModuleName").unwrap_or_else(|| "?".to_string());
    let code = field("ExceptionCode").unwrap_or_else(|| "?".to_string());
    format!("Windows: audiodg.exe crashed in {module} (exception {code})")
  } else if provider.contains("Defender") {
    let threat = field("Threat Name").unwrap_or_else(|| "?".to_string());
    let path = field("Path").unwrap_or_else(|| "?".to_string());
    let action = if id == "1117" { "acted on" } else { "detected" };
    format!("Windows: Defender {action} {threat} in {path}")
  } else {
    let file = field("FileNameBuffer").or_else(|| field("File Name")).unwrap_or_else(|| "?".to_string());
    format!("Windows: Code Integrity event {id} for {file}")
  };
  ApoNote { time, text }
}

/// The event as XML.
///
/// # Safety
/// `event` must be a live event handle from `EvtNext`.
unsafe fn render_xml(event: EVT_HANDLE) -> Option<String> {
  let mut used = 0u32;
  let mut count = 0u32;
  // First call only asks for the size (it fails with "insufficient buffer").
  let _ = EvtRender(None, event, EvtRenderEventXml.0, 0, None, &mut used, &mut count);
  let mut buf = vec![0u16; (used as usize).div_ceil(2)];
  let bytes = u32::try_from(buf.len() * 2).ok()?;
  EvtRender(None, event, EvtRenderEventXml.0, bytes, Some(buf.as_mut_ptr().cast()), &mut used, &mut count).ok()?;
  let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
  Some(String::from_utf16_lossy(buf.get(..len)?))
}

/// `SystemTime='...'` and the first `<Data>` of a classic event's XML.
fn parse(xml: &str) -> Option<ApoNote> {
  let time = between(xml, "SystemTime='", "'").unwrap_or_default();
  let text = between(xml, "<Data>", "</Data>")?;
  Some(ApoNote { time: time.to_string(), text: unescape(text) })
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
  let from = s.find(start)? + start.len();
  let rest = s.get(from..)?;
  rest.get(..rest.find(end)?)
}

fn unescape(s: &str) -> String {
  s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  #[test]
  fn note_is_read_from_event_xml() {
    let xml = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='GamersEqualizer'/>\
               <EventID Qualifiers='0'>1</EventID><TimeCreated SystemTime='2026-11-10T19:29:41.1234567Z'/></System>\
               <EventData><Data>audiodg pid 42: Mic chain: stream 48000 Hz, 2 ch &amp; more &lt;x&gt;</Data></EventData></Event>";
    let note = parse(xml).unwrap();
    assert_eq!(note.time, "2026-11-10T19:29:41.1234567Z");
    assert_eq!(note.text, "audiodg pid 42: Mic chain: stream 48000 Hz, 2 ch & more <x>");
    assert_eq!(parse("<Event><System/></Event>"), None, "no Data: no note");
  }

  #[test]
  fn windows_events_become_one_line_each() {
    let crash = "<Event><System><Provider Name='Application Error' Guid='{a}'/><EventID>1000</EventID>\
                 <TimeCreated SystemTime='2026-11-11T07:00:00.1Z'/></System><EventData><Data Name='AppName'>audiodg.exe</Data>\
                 <Data Name='ModuleName'>gamersequalizer_apo.dll</Data><Data Name='ExceptionCode'>c0000005</Data></EventData></Event>";
    let note = windows_note(crash);
    assert_eq!(note.time, "2026-11-11T07:00:00.1Z");
    assert_eq!(note.text, "Windows: audiodg.exe crashed in gamersequalizer_apo.dll (exception c0000005)");
    let defender =
      "<Event><System><Provider Name='Microsoft-Windows-Windows Defender' Guid='{b}'/><EventID>1116</EventID>\
                    </System><EventData><Data Name='Threat Name'>Trojan:Win32/Cloxer</Data>\
                    <Data Name='Path'>file:_C:\\x\\gamersequalizer.exe</Data></EventData></Event>";
    assert_eq!(
      windows_note(defender).text,
      "Windows: Defender detected Trojan:Win32/Cloxer in file:_C:\\x\\gamersequalizer.exe"
    );
  }
}
