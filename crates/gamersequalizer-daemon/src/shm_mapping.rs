//! The shared-memory mapping between the window and the service (`Local\`, `SHM_NAME`).

use gamersequalizer_common::shm::{SHM_NAME, ShmLayout};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

/// Open the daemon <-> window mapping; a new one (nothing else runs) gets the defaults, the
/// Windows volumes and then the user's saved choices from `settings_dir`.
pub(crate) fn open_or_create_shm(settings_dir: &Path) -> (Option<HANDLE>, Option<NonNull<ShmLayout>>) {
  // SAFETY: the name is NUL-terminated and outlives the calls; the view is the layout's size or null (checked, the
  // handle is then closed, as for a mapping of another layout). Defaults are written only into a mapping no one set up
  // yet (magic not set), before the pointer is handed out.
  unsafe {
    let wide_name: Vec<u16> = SHM_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let size_bytes = ShmLayout::total_size_bytes() as u32;

    let handle =
      match CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, size_bytes, PCWSTR(wide_name.as_ptr())) {
        Ok(h) => h,
        Err(_) => return (None, None),
      };

    let map_ptr = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size_bytes as usize);
    if map_ptr.Value.is_null() {
      let _ = CloseHandle(handle);
      return (None, None);
    }

    let ptr = NonNull::new(map_ptr.Value as *mut ShmLayout);
    if let Some(p) = ptr {
      let header = &mut (*p.as_ptr()).header;
      if header.magic == gamersequalizer_common::shm::SHM_MAGIC
        && let Some(e) = header.layout_mismatch()
      {
        log::warn!("[MicEQ] {SHM_NAME} has {e}: running without it");
        let _ = UnmapViewOfFile(map_ptr);
        let _ = CloseHandle(handle);
        return (None, None);
      }
      if header.magic != gamersequalizer_common::shm::SHM_MAGIC {
        header.init_defaults();
        if let Some((vol, mute)) = crate::win_audio::chatmix::get_system_master_volume() {
          header.master_volume_permille.store((vol * 1000.0).round() as u32, Ordering::Relaxed);
          header.master_muted.store(mute, Ordering::Relaxed);
        }
        if let Some((vol, mute)) = crate::win_audio::chatmix::get_system_mic_volume() {
          header.mic_volume_permille.store((vol * 1000.0).round() as u32, Ordering::Relaxed);
          header.mic_muted.store(mute, Ordering::Relaxed);
        }
        crate::ui::audio_settings::AudioSettings::load_or_default(settings_dir).apply_to_header(header);
      }
    }
    (Some(handle), ptr)
  }
}
