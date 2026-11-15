//! Windows system clipboard bridge using Win32 API.
//! Used because eframe default features are disabled to keep dependencies lean.

use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_UNICODETEXT: u32 = 13;
/// Clipboard managers and other apps hold the clipboard open for a moment: try this often.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Copies `text` to the Windows clipboard as UTF-16 text (`CF_UNICODETEXT`). A failure is
/// logged here, so callers that have nothing to show may ignore the result.
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
  let result = copy_utf16(text);
  if let Err(e) = &result {
    log::warn!("[Clipboard] {e}");
  }
  result
}

fn copy_utf16(text: &str) -> Result<(), String> {
  let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
  let bytes = utf16.len() * std::mem::size_of::<u16>();

  // SAFETY: plain allocation; freed below on every error path, owned by the system once
  // SetClipboardData succeeds.
  let hglobal: HGLOBAL =
    unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|e| format!("GlobalAlloc failed: {e}"))?;
  let free = |h: HGLOBAL| {
    // SAFETY: `h` came from GlobalAlloc above and the system does not own it yet.
    let _ = unsafe { GlobalFree(Some(h)) };
  };
  // SAFETY: `hglobal` is a live movable block of `bytes` bytes.
  let ptr = unsafe { GlobalLock(hglobal) };
  if ptr.is_null() {
    free(hglobal);
    return Err("GlobalLock returned null".to_string());
  }
  // SAFETY: `ptr` points to `bytes` writable bytes, exactly `utf16.len()` u16 values; the
  // block and the Vec do not overlap.
  unsafe { std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len()) };
  // SAFETY: balances the GlobalLock above. Its "error" is only "now unlocked".
  let _ = unsafe { GlobalUnlock(hglobal) };

  let mut opened = Err(String::new());
  for attempt in 0..OPEN_ATTEMPTS {
    if attempt > 0 {
      std::thread::sleep(OPEN_RETRY_DELAY);
    }
    // SAFETY: no preconditions; closed below on every path once it succeeded.
    opened = unsafe { OpenClipboard(None) }.map_err(|e| format!("OpenClipboard failed: {e}"));
    if opened.is_ok() {
      break;
    }
  }
  if let Err(e) = opened {
    free(hglobal);
    return Err(e);
  }

  // SAFETY: the clipboard is open on this thread (OpenClipboard succeeded above).
  let result = unsafe { EmptyClipboard() }.map_err(|e| format!("EmptyClipboard failed: {e}")).and_then(|()| {
    // SAFETY: clipboard open and emptied; `hglobal` holds NUL-terminated UTF-16.
    unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))) }
      .map(|_| ())
      .map_err(|e| format!("SetClipboardData failed: {e}"))
  });
  if result.is_err() {
    free(hglobal);
  }
  // SAFETY: the clipboard is open on this thread.
  let _ = unsafe { CloseClipboard() };
  result
}
