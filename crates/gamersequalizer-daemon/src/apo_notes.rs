//! Notes our APO writes from inside audiodg into the Windows Application log (source
//! `GamersEqualizer`, `gamersequalizer-apo/src/event_log.rs`): which stream it started on and
//! why it passes audio through unchanged. The service copies new ones into its own log and
//! hands the newest to the window, so "it does not run" comes with the reason.

use windows::core::{w, HRESULT};
use windows::Win32::System::EventLog::{
  EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath, EvtQueryReverseDirection, EvtRender, EvtRenderEventXml, EVT_HANDLE,
};

/// Newest notes read per check.
const MAX_NOTES: usize = 10;
/// How long EvtNext may wait for the (local) log, ms.
const NEXT_TIMEOUT_MS: u32 = 1000;
/// `ERROR_NO_MORE_ITEMS`: the query matched nothing (not a failure).
const NO_MORE_ITEMS: u32 = 259;

/// One note: when (UTC, as Windows writes it, `2026-11-10T19:29:41.12Z`) and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApoNote {
  pub time: String,
  pub text: String,
}

/// Newest first (empty: there are none). None when the log could not be read (a timeout, no
/// access): the caller keeps what it had, so old notes are not logged again later.
pub fn newest() -> Option<Vec<ApoNote>> {
  let mut notes = Vec::new();
  // SAFETY: every handle these calls return is closed below; buffers are owned here.
  unsafe {
    let flags = EvtQueryChannelPath.0 | EvtQueryReverseDirection.0;
    let query = EvtQuery(None, w!("Application"), w!("*[System[Provider[@Name='GamersEqualizer']]]"), flags).ok()?;
    let mut raw = [0isize; MAX_NOTES];
    let mut returned = 0u32;
    let read = match EvtNext(query, &mut raw, NEXT_TIMEOUT_MS, 0, &mut returned) {
      Ok(()) => true,
      Err(e) => e.code() == HRESULT::from_win32(NO_MORE_ITEMS),
    };
    if read {
      for &handle in raw.iter().take(returned as usize) {
        let event = EVT_HANDLE(handle);
        if let Some(note) = render_xml(event).as_deref().and_then(parse) {
          notes.push(note);
        }
        let _ = EvtClose(event);
      }
    }
    let _ = EvtClose(query);
    read.then_some(notes)
  }
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
}
