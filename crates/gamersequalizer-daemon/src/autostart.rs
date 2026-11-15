use log::info;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
  HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
  RegQueryValueExW, RegSetValueExW,
};
use windows::core::{PCWSTR, w};

const RUN_KEY_SUBKEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE_NAME: PCWSTR = w!("GamersEqualizer");

/// Check if GamersEqualizer is configured to automatically start with Windows
pub fn is_autostart_enabled() -> bool {
  // SAFETY: static NUL-terminated names; the value buffer is a local whose byte size is passed; the key is closed
  // before returning.
  unsafe {
    let mut hkey = Default::default();
    if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY_SUBKEY, Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return false;
    }

    let mut buf = [0u16; 1024];
    let mut buf_size = (buf.len() * 2) as u32;
    let mut reg_type = REG_VALUE_TYPE::default();

    let res = RegQueryValueExW(
      hkey,
      RUN_VALUE_NAME,
      None,
      Some(&mut reg_type),
      Some(buf.as_mut_ptr() as *mut u8),
      Some(&mut buf_size),
    );

    let _ = RegCloseKey(hkey);
    res == ERROR_SUCCESS && reg_type == REG_SZ
  }
}

/// Enable or disable starting in background with Windows
pub fn set_autostart(enable: bool) -> Result<(), String> {
  // SAFETY: static NUL-terminated names; the value bytes view `wide_cmd`, which outlives the call; the key is
  // closed once before returning.
  unsafe {
    let mut hkey = Default::default();
    let status = RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY_SUBKEY, Some(0), KEY_WRITE, &mut hkey);
    if status != ERROR_SUCCESS {
      return Err(format!("Failed to open Run registry key: {:?}", status));
    }

    let result = if enable {
      // The installed copy once there is one: the unzipped folder may be deleted.
      let exe_path = crate::win_audio::apo_install::autostart_exe().ok_or("the app's exe path is unknown")?;
      // Launch headless daemon on login
      let cmd_str = format!("\"{}\" --daemon", exe_path.to_string_lossy());
      let wide_cmd: Vec<u16> = cmd_str.encode_utf16().chain(std::iter::once(0)).collect();
      let bytes = std::slice::from_raw_parts(wide_cmd.as_ptr() as *const u8, wide_cmd.len() * 2);

      let set_res = RegSetValueExW(hkey, RUN_VALUE_NAME, Some(0), REG_SZ, Some(bytes));

      if set_res == ERROR_SUCCESS {
        info!("[GamersEqualizer] Windows autostart enabled: {}", cmd_str);
        Ok(())
      } else {
        Err(format!("Failed to set registry value: {:?}", set_res))
      }
    } else {
      let del_res = RegDeleteValueW(hkey, RUN_VALUE_NAME);
      if del_res == ERROR_SUCCESS || del_res == ERROR_FILE_NOT_FOUND {
        info!("[GamersEqualizer] Windows autostart disabled.");
        Ok(())
      } else {
        Err(format!("Failed to delete registry value: {:?}", del_res))
      }
    };

    let _ = RegCloseKey(hkey);
    result
  }
}

/// Rewrites an enabled autostart entry, so it follows the app into its installed place
/// (`autostart_exe`). No entry: nothing happens.
pub fn repoint() {
  if is_autostart_enabled()
    && let Err(e) = set_autostart(true)
  {
    log::warn!("[GamersEqualizer] could not update the autostart entry: {e}");
  }
}
