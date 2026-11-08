//! Facts about a game executable from local launcher metadata (no network): Steam app
//! manifests, Epic manifests, GOG / Ubisoft registry entries, Microsoft Store package folders,
//! and the exe's own version info. Every source is optional; failures just yield fewer facts.

use super::game_match::GameFacts;
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Registry::{
  RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
};

const EPIC_MANIFESTS: &str = r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests";
const GOG_GAMES_KEY: &str = r"SOFTWARE\WOW6432Node\GOG.com\Games";
const UBISOFT_INSTALLS_KEY: &str = r"SOFTWARE\WOW6432Node\Ubisoft\Launcher\Installs";
/// Bounds for registry enumeration (a launcher never has this many games).
const MAX_REG_SUBKEYS: u32 = 4096;

pub fn gather_facts(full_path: &str) -> GameFacts {
  let path = Path::new(full_path);
  let exe_name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
  let mut facts = GameFacts { exe_name, ..Default::default() };
  let lower_path = full_path.to_lowercase();

  steam(path, &mut facts);
  epic(&lower_path, &mut facts);
  registry_installs(GOG_GAMES_KEY, "path", "gameName", "3", &lower_path, &mut facts);
  registry_installs(UBISOFT_INSTALLS_KEY, "InstallDir", "", "8", &lower_path, &mut facts);
  microsoft_store(path, &mut facts);
  for name in version_info_names(full_path) {
    if !facts.names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
      facts.names.push(name);
    }
  }
  facts
}

// --- Steam ---------------------------------------------------------------------------------

/// `<lib>\steamapps\common\<installdir>\...\game.exe` -> `appmanifest_<id>.acf` with that installdir.
fn steam(path: &Path, facts: &mut GameFacts) {
  let components: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
  let Some(common) = components.windows(2).position(|w| {
    w.first().is_some_and(|a| a.eq_ignore_ascii_case("steamapps"))
      && w.get(1).is_some_and(|b| b.eq_ignore_ascii_case("common"))
  }) else {
    return;
  };
  let Some(install_dir) = components.get(common + 2) else {
    return;
  };
  let steamapps: PathBuf = components.iter().take(common + 1).collect();
  let Ok(entries) = std::fs::read_dir(&steamapps) else {
    return;
  };
  for entry in entries.flatten() {
    let name = entry.file_name().to_string_lossy().into_owned();
    if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
      continue;
    }
    let Ok(text) = std::fs::read_to_string(entry.path()) else {
      continue;
    };
    if acf_value(&text, "installdir").is_some_and(|d| d.eq_ignore_ascii_case(install_dir)) {
      if let Some(id) = acf_value(&text, "appid") {
        facts.launcher_ids.push(format!("1_{id}"));
      }
      if let Some(title) = acf_value(&text, "name") {
        facts.names.push(title);
      }
      facts.from_launcher = true;
      return;
    }
  }
  // Under steamapps\common but no manifest: still a Steam game.
  facts.from_launcher = true;
  facts.names.push(install_dir.clone());
}

/// Value of a top-level `"key"  "value"` line in a Valve KeyValues file.
pub fn acf_value(text: &str, key: &str) -> Option<String> {
  text.lines().find_map(|line| {
    let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
    let k = parts.next()?;
    let v = parts.next()?;
    k.eq_ignore_ascii_case(key).then(|| v.to_string())
  })
}

// --- Epic ----------------------------------------------------------------------------------

fn epic(lower_path: &str, facts: &mut GameFacts) {
  let Ok(entries) = std::fs::read_dir(EPIC_MANIFESTS) else {
    return;
  };
  for entry in entries.flatten() {
    let Ok(text) = std::fs::read_to_string(entry.path()) else {
      continue;
    };
    let Ok(item) = serde_json::from_str::<serde_json::Value>(&text) else {
      continue;
    };
    let field = |k: &str| item.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let location = field("InstallLocation").to_lowercase();
    if location.is_empty() || !lower_path.starts_with(&location) {
      continue;
    }
    for id in ["CatalogItemId", "AppName", "CatalogNamespace"].map(field).into_iter().filter(|s| !s.is_empty()) {
      facts.launcher_ids.push(format!("9_{id}"));
    }
    let title = field("DisplayName");
    if !title.is_empty() {
      facts.names.push(title);
    }
    facts.from_launcher = true;
    return;
  }
}

// --- GOG / Ubisoft (registry) ----------------------------------------------------------------

/// Each subkey of `key` is one installed game (subkey name = launcher id). A game matches when
/// the exe lies inside its `path_value` folder.
fn registry_installs(
  key: &str,
  path_value: &str,
  title_value: &str,
  prefix: &str,
  lower_path: &str,
  facts: &mut GameFacts,
) {
  for id in registry_subkeys(key) {
    let sub = format!("{key}\\{id}");
    let Some(dir) = reg_string(&sub, path_value) else {
      continue;
    };
    let dir = dir.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    if dir.is_empty() || !lower_path.starts_with(&dir) {
      continue;
    }
    facts.launcher_ids.push(format!("{prefix}_{id}"));
    if let Some(title) = (!title_value.is_empty()).then(|| reg_string(&sub, title_value)).flatten() {
      facts.names.push(title);
    }
    facts.from_launcher = true;
    return;
  }
}

fn registry_subkeys(key: &str) -> Vec<String> {
  let key_w = wide(key);
  let mut hkey = HKEY::default();
  let mut out = Vec::new();
  // SAFETY: key handle is closed below; buffers are locals sized as passed.
  unsafe {
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_w.as_ptr()), Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return out;
    }
    for index in 0..MAX_REG_SUBKEYS {
      let mut name = [0u16; 256];
      let mut len = name.len() as u32;
      let rc = RegEnumKeyExW(hkey, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None);
      if rc != ERROR_SUCCESS {
        break;
      }
      out.push(String::from_utf16_lossy(name.get(..len as usize).unwrap_or_default()));
    }
    let _ = RegCloseKey(hkey);
  }
  out
}

fn reg_string(key: &str, value: &str) -> Option<String> {
  let (key_w, value_w) = (wide(key), wide(value));
  let mut buf = [0u16; 1024];
  let mut size = (buf.len() * 2) as u32;
  // SAFETY: buffer and size describe a local array.
  unsafe {
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      PCWSTR(key_w.as_ptr()),
      PCWSTR(value_w.as_ptr()),
      RRF_RT_REG_SZ,
      None,
      Some(buf.as_mut_ptr().cast()),
      Some(&mut size),
    )
    .ok()
    .ok()?;
  }
  let units = buf.iter().take((size as usize) / 2).copied().take_while(|&u| u != 0).collect::<Vec<u16>>();
  Some(String::from_utf16_lossy(&units))
}

// --- Microsoft Store / Xbox app ----------------------------------------------------------------

/// `...\WindowsApps\<Name>_<Version>_<Arch>_<Resource>_<PublisherId>\...` -> family
/// `NAME_PUBLISHERID` (Sonar stores it uppercase). `...\XboxGames\<Title>\Content\...` -> title.
fn microsoft_store(path: &Path, facts: &mut GameFacts) {
  let components: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
  for (i, c) in components.iter().enumerate() {
    let Some(next) = components.get(i + 1) else {
      break;
    };
    if c.eq_ignore_ascii_case("WindowsApps") {
      let parts: Vec<&str> = next.split('_').collect();
      if let (Some(name), Some(publisher)) = (parts.first(), parts.last()) {
        if parts.len() >= 5 {
          facts.launcher_ids.push(format!("10_{name}_{publisher}").to_uppercase());
          facts.from_launcher = true;
        }
      }
      return;
    }
    if c.eq_ignore_ascii_case("XboxGames") {
      facts.names.push(next.clone());
      facts.from_launcher = true;
      return;
    }
  }
}

// --- exe version info ------------------------------------------------------------------------

/// ProductName and FileDescription of the exe ("Tom Clancy's The Division 2").
fn version_info_names(full_path: &str) -> Vec<String> {
  let path_w = wide(full_path);
  let mut out = Vec::new();
  // SAFETY: the version block is a local buffer of the size the API asked for; pointers
  // returned by VerQueryValueW point into it and are read within its lifetime.
  unsafe {
    let size = GetFileVersionInfoSizeW(PCWSTR(path_w.as_ptr()), None);
    if size == 0 || size > 16 * 1024 * 1024 {
      return out;
    }
    let mut block = vec![0u8; size as usize];
    if GetFileVersionInfoW(PCWSTR(path_w.as_ptr()), None, size, block.as_mut_ptr().cast()).is_err() {
      return out;
    }
    // First language/codepage pair of the translation table.
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let translation = wide(r"\VarFileInfo\Translation");
    if !VerQueryValueW(block.as_ptr().cast(), PCWSTR(translation.as_ptr()), &mut ptr, &mut len).as_bool()
      || len < 4
      || ptr.is_null()
    {
      return out;
    }
    let lang = std::ptr::read_unaligned(ptr as *const u16);
    let codepage = std::ptr::read_unaligned((ptr as *const u16).add(1));
    for field in ["ProductName", "FileDescription"] {
      let query = wide(&format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\{field}"));
      let mut value: *mut core::ffi::c_void = std::ptr::null_mut();
      let mut chars = 0u32;
      if VerQueryValueW(block.as_ptr().cast(), PCWSTR(query.as_ptr()), &mut value, &mut chars).as_bool()
        && !value.is_null()
        && chars > 0
      {
        let units = std::slice::from_raw_parts(value as *const u16, chars as usize);
        let text = String::from_utf16_lossy(units).trim_end_matches('\0').trim().to_string();
        if !text.is_empty() && !out.contains(&text) {
          out.push(text);
        }
      }
    }
  }
  out
}

fn wide(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  #[test]
  fn steam_manifest_maps_install_dir_to_app_id() {
    let lib = std::env::temp_dir().join(format!("ge_steam_test_{}", std::process::id()));
    let game_dir = lib.join("steamapps").join("common").join("Tom Clancy's The Division 2");
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::write(
      lib.join("steamapps").join("appmanifest_2221490.acf"),
      "\"AppState\"\n{\n\t\"appid\"\t\t\"2221490\"\n\t\"name\"\t\t\"Tom Clancy's The Division 2\"\n\t\"installdir\"\t\t\"Tom Clancy's The Division 2\"\n}\n",
    )
    .unwrap();
    let facts = gather_facts(&game_dir.join("TheDivision2.exe").to_string_lossy());
    assert_eq!(facts.exe_name, "thedivision2.exe");
    assert_eq!(facts.launcher_ids, vec!["1_2221490"]);
    assert_eq!(facts.names.first().map(String::as_str), Some("Tom Clancy's The Division 2"));
    assert!(facts.from_launcher);
    let _ = std::fs::remove_dir_all(&lib);
  }

  #[test]
  fn store_package_folder_gives_family_name() {
    let mut facts = GameFacts::default();
    microsoft_store(
      Path::new(r"C:\Program Files\WindowsApps\Microsoft.624F8B84B80_3.0.1.0_x64__8wekyb3d8bbwe\ForzaHorizon5.exe"),
      &mut facts,
    );
    assert_eq!(facts.launcher_ids, vec!["10_MICROSOFT.624F8B84B80_8WEKYB3D8BBWE"]);
  }

  #[test]
  fn plain_apps_have_no_launcher_facts() {
    let facts = gather_facts(r"C:\Windows\System32\notepad.exe");
    assert!(!facts.from_launcher && facts.launcher_ids.is_empty());
  }
}
