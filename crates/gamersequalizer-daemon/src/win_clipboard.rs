//! Windows system clipboard bridge using Win32 API.
//! Used because eframe default features are disabled to keep dependencies lean.

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

/// Copies `text` to the Windows clipboard as UTF-16 text (`CF_UNICODETEXT`).
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
  let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
  let bytes = utf16.len() * std::mem::size_of::<u16>();

  unsafe {
    let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| format!("GlobalAlloc failed: {e}"))?;
    let ptr = GlobalLock(hglobal);
    if ptr.is_null() {
      let _ = GlobalFree(Some(hglobal));
      return Err("GlobalLock returned null".to_string());
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
    let _ = GlobalUnlock(hglobal);

    if let Err(e) = OpenClipboard(None) {
      let _ = GlobalFree(Some(hglobal));
      return Err(format!("OpenClipboard failed: {e}"));
    }

    if let Err(e) = EmptyClipboard() {
      let _ = CloseClipboard();
      let _ = GlobalFree(Some(hglobal));
      return Err(format!("EmptyClipboard failed: {e}"));
    }

    if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))) {
      let _ = CloseClipboard();
      let _ = GlobalFree(Some(hglobal));
      return Err(format!("SetClipboardData failed: {e}"));
    }

    let _ = CloseClipboard();
  }

  Ok(())
}
