//! Reading the endpoint and COM registration keys under HKLM.

use super::*;

pub(super) fn hklm_subkeys(key: &str) -> Vec<String> {
  /// Bound for the enumeration (a PC never has this many audio endpoints).
  const MAX_SUBKEYS: u32 = 4096;
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let mut hkey = HKEY::default();
  let mut out = Vec::new();
  // SAFETY: the handle is closed below; the name buffer is a local array of the size passed.
  unsafe {
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_w.as_ptr()), Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return out;
    }
    for index in 0..MAX_SUBKEYS {
      let mut name = [0u16; 256];
      let mut len = name.len() as u32;
      if RegEnumKeyExW(hkey, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
        break;
      }
      out.push(String::from_utf16_lossy(name.get(..len as usize).unwrap_or_default()));
    }
    let _ = RegCloseKey(hkey);
  }
  out
}

pub(super) fn hklm_key_exists(key: &str) -> bool {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let mut hkey = HKEY::default();
  // SAFETY: NUL-terminated key name owned by this frame; the handle is closed right away.
  unsafe {
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_w.as_ptr()), Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return false;
    }
    let _ = RegCloseKey(hkey);
  }
  true
}

pub(super) fn endpoints_root(flow: Flow) -> String {
  format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\{}", flow.as_str())
}

pub(super) fn endpoint_has_any_clsid(flow: Flow, guid: &str) -> bool {
  let lists = endpoint_fx_lists(flow, guid);
  lists.iter().any(|list| flow.clsids().iter().any(|clsid| list.contains(clsid)))
}

/// True only when all required CLSIDs sit in composite slots whose streaming modes include the
/// Default processing mode and whose own legacy value (SFX `,1`/`,5`, MFX `,6`, EFX `,7`) does not
/// shadow them. A legacy value on another slot is harmless (measured: `,5`/`,6` left `,15` running).
pub(super) fn endpoint_has_apo(flow: Flow, guid: &str) -> bool {
  let key = format!("{}\\{guid}\\FxProperties", endpoints_root(flow));
  let slots = FX_LIST_VALUES.map(|n| read_string_value(&key, n));
  let modes = FX_MODE_VALUES.map(|(_, n)| read_string_value(&key, n));
  let legacy = FX_LEGACY_VALUES.map(|(_, n)| read_string_value(&key, n));
  endpoint_fx_configured(flow, &slots, &modes, &legacy)
}

pub(super) fn endpoint_fx_configured(
  flow: Flow,
  slots: &[Option<String>; 3],
  modes: &[Option<String>; 3],
  legacy: &[Option<String>; 4],
) -> bool {
  let in_use = |raw: &Option<String>| {
    raw
      .as_deref()
      .is_some_and(|raw| raw.split('\n').map(str::trim).any(|s| !s.is_empty() && !s.eq_ignore_ascii_case(NULL_GUID)))
  };
  // FX_LEGACY_VALUES order: LFX, SFX, MFX, EFX; slots: SFX, MFX, EFX.
  let [lfx, sfx, mfx, efx] = legacy;
  let shadowed = [in_use(lfx) || in_use(sfx), in_use(mfx), in_use(efx)];
  flow.clsids().iter().all(|clsid| {
    slots.iter().zip(modes.iter()).zip(shadowed).any(|((slot, mode), shadowed)| {
      let has_clsid = slot.as_deref().is_some_and(|s| s.to_ascii_lowercase().contains(clsid));
      let has_mode = mode.as_deref().is_some_and(|m| m.to_ascii_lowercase().contains(DEFAULT_PROCESSING_MODE));
      has_clsid && has_mode && !shadowed
    })
  })
}

/// The endpoint's composite effect lists, lowercased. Only these: the install script writes
/// nothing else (a legacy value is copied into the composite list, never edited).
pub(super) fn endpoint_fx_lists(flow: Flow, guid: &str) -> Vec<String> {
  let key = format!("{}\\{guid}\\FxProperties", endpoints_root(flow));
  FX_LIST_VALUES.iter().filter_map(|value| read_string_value(&key, value)).map(|l| l.to_ascii_lowercase()).collect()
}

/// REG_SZ or REG_MULTI_SZ as one string (NULs become '\n').
pub(super) fn read_string_value(key: &str, value: &str) -> Option<String> {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
  let flags = RRF_RT_REG_SZ | RRF_RT_REG_MULTI_SZ;
  let mut size = 0u32;
  // SAFETY: first call only queries the size; the second writes into a buffer of that size.
  unsafe {
    let key_p = PCWSTR(key_w.as_ptr());
    let value_p = PCWSTR(value_w.as_ptr());
    RegGetValueW(HKEY_LOCAL_MACHINE, key_p, value_p, flags, None, None, Some(&mut size)).ok().ok()?;
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    RegGetValueW(HKEY_LOCAL_MACHINE, key_p, value_p, flags, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
      .ok()
      .ok()?;
    Some(String::from_utf16_lossy(&buf).replace('\0', "\n"))
  }
}

pub(super) fn read_dword_value(key: &str, value: &str) -> Option<u32> {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
  let mut data = 0u32;
  let mut size = std::mem::size_of::<u32>() as u32;
  // SAFETY: the out buffer is a local u32 of the size passed.
  unsafe {
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      PCWSTR(key_w.as_ptr()),
      PCWSTR(value_w.as_ptr()),
      RRF_RT_REG_DWORD,
      None,
      Some(&mut data as *mut u32 as *mut _),
      Some(&mut size),
    )
    .ok()
    .ok()?;
  }
  Some(data)
}
