//! Small shell helpers for the UI: open a web page in the user's browser (the app itself has
//! no network code) and the native "open file" dialog.

use std::path::PathBuf;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::UI::Controls::Dialogs::{
  GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Only https pages are opened (ShellExecute would also run local programs).
pub fn open_url(url: &str) {
  if !url.starts_with("https://") {
    log::warn!("[Shell] refusing to open {url}");
    return;
  }
  let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: NUL-terminated string owned by this frame.
  unsafe {
    ShellExecuteW(None, w!("open"), PCWSTR(url_w.as_ptr()), None, None, SW_SHOWNORMAL);
  }
}

/// Windows Settings > System > Sound (output devices; spatial sound is under each device).
pub fn open_sound_settings() {
  // SAFETY: constant NUL-terminated strings.
  unsafe {
    ShellExecuteW(None, w!("open"), w!("ms-settings:sound"), None, None, SW_SHOWNORMAL);
  }
}

/// Windows Settings page of the default output device, with its "Spatial sound" choice.
pub fn open_spatial_sound_settings() {
  // SAFETY: constant NUL-terminated strings.
  unsafe {
    ShellExecuteW(None, w!("open"), w!("ms-settings:sound-defaultoutputproperties"), None, None, SW_SHOWNORMAL);
  }
}

/// Modal "open" dialog for `.txt` files. None if cancelled.
pub fn pick_text_file(title: &str) -> Option<PathBuf> {
  let mut file = [0u16; 1024];
  let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
  // Pairs of "label\0pattern\0", ended by an extra NUL.
  let filter: Vec<u16> = "Text files (*.txt)\0*.txt\0All files\0*.*\0\0".encode_utf16().collect();
  let mut ofn = OPENFILENAMEW {
    lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
    lpstrFilter: PCWSTR(filter.as_ptr()),
    lpstrFile: PWSTR(file.as_mut_ptr()),
    nMaxFile: file.len() as u32,
    lpstrTitle: PCWSTR(title_w.as_ptr()),
    Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
    ..Default::default()
  };
  // SAFETY: every pointer in `ofn` refers to a buffer that lives until the call returns.
  let ok = unsafe { GetOpenFileNameW(&mut ofn) }.as_bool();
  if !ok {
    return None;
  }
  let len = file.iter().position(|&c| c == 0).unwrap_or(file.len());
  Some(PathBuf::from(String::from_utf16_lossy(file.get(..len).unwrap_or_default())))
}
