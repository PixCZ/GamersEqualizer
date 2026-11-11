//! UI translations without a dependency: the English text is the key, each language is a
//! static table (`i18n_cs.rs`). A missing entry falls back to English, so a new string never
//! breaks the UI; the test at the bottom scans the sources and fails on untranslated `tr()`
//! calls, so it never ships untranslated either.
//!
//! Adding a language: a `Lang` variant, its table file, a row in `LANGUAGES`, a branch in
//! `table()` and in `system_language()`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Languages the UI is translated to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
  En,
  Cs,
}

/// What the user picked in Settings (stored in `ui_prefs.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LanguagePref {
  English,
  Czech,
  /// Windows display language, English when it is not translated. Last: serde's catch-all.
  #[default]
  #[serde(other)]
  System,
}

/// (preference, name shown in the picker, in its own language).
pub const LANGUAGES: [(LanguagePref, &str); 2] = [(LanguagePref::English, "English"), (LanguagePref::Czech, "Čeština")];

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Switch the UI language (takes effect on the next frame).
pub fn set_language(pref: LanguagePref) {
  let lang = match pref {
    LanguagePref::System => system_language(),
    LanguagePref::English => Lang::En,
    LanguagePref::Czech => Lang::Cs,
  };
  CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
  match CURRENT.load(Ordering::Relaxed) {
    1 => Lang::Cs,
    _ => Lang::En,
  }
}

/// Translate an English UI text; unknown texts (profile names, device names) pass through.
pub fn tr(en: &str) -> &str {
  match table(current()) {
    Some(map) => map.get(en).copied().unwrap_or(en),
    None => en,
  }
}

/// Marks a text in a constant as a translation key (translated later with `tr`), so the
/// coverage test finds it. Returns its argument.
pub const fn tk(en: &'static str) -> &'static str {
  en
}

/// Translate a template and fill it: `{}` takes the next argument, `{0}`, `{1}` a fixed one
/// (a translation may reorder them). Missing arguments leave the placeholder empty.
pub fn trf(en: &str, args: &[&dyn std::fmt::Display]) -> String {
  fill(tr(en), args)
}

/// Windows display language mapped to a translated one.
pub fn system_language() -> Lang {
  // SAFETY: no arguments, returns the LANGID of the user's display language.
  let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
  match langid & 0x3ff {
    0x05 => Lang::Cs, // LANG_CZECH
    _ => Lang::En,
  }
}

fn table(lang: Lang) -> Option<&'static HashMap<&'static str, &'static str>> {
  static CS: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
  match lang {
    Lang::En => None,
    Lang::Cs => Some(CS.get_or_init(|| crate::i18n_cs::CS.iter().copied().collect())),
  }
}

fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
  let mut out = String::with_capacity(template.len() + 16);
  let mut next = 0;
  let mut rest = template;
  while let Some(open) = rest.find('{') {
    out.push_str(&rest[..open]);
    let after = &rest[open + 1..];
    match after.find('}') {
      Some(close) if after[..close].chars().all(|c| c.is_ascii_digit()) => {
        let index = if close == 0 {
          next += 1;
          next - 1
        } else {
          after[..close].parse().unwrap_or(usize::MAX)
        };
        if let Some(arg) = args.get(index) {
          out.push_str(&arg.to_string());
        }
        rest = &after[close + 1..];
      }
      _ => {
        out.push('{');
        rest = after;
      }
    }
  }
  out.push_str(rest);
  out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
  use super::*;

  /// Files whose `tr("...")` / `trf("...")` literals must all be translated.
  const SOURCES: [(&str, &str); 19] = [
    ("output_eq/game_presets.rs", include_str!("output_eq/game_presets.rs")),
    ("tray.rs", include_str!("tray.rs")),
    ("ui/setup.rs", include_str!("ui/setup.rs")),
    ("health.rs", include_str!("health.rs")),
    ("ui/apo_banner.rs", include_str!("ui/apo_banner.rs")),
    ("ui/eq_editor.rs", include_str!("ui/eq_editor.rs")),
    ("ui/eq_graph.rs", include_str!("ui/eq_graph.rs")),
    ("ui/header.rs", include_str!("ui/header.rs")),
    ("ui/mic_presets.rs", include_str!("ui/mic_presets.rs")),
    ("ui/profile_bar.rs", include_str!("ui/profile_bar.rs")),
    ("ui/tab_eq.rs", include_str!("ui/tab_eq.rs")),
    ("ui/tab_headphones.rs", include_str!("ui/tab_headphones.rs")),
    ("ui/tab_log.rs", include_str!("ui/tab_log.rs")),
    ("ui/tab_mic.rs", include_str!("ui/tab_mic.rs")),
    ("ui/tab_mixer.rs", include_str!("ui/tab_mixer.rs")),
    ("ui/tab_settings.rs", include_str!("ui/tab_settings.rs")),
    ("ui/theme.rs", include_str!("ui/theme.rs")),
    ("output_eq/autoeq.rs", include_str!("output_eq/autoeq.rs")),
    ("win_audio/dolby_atmos.rs", include_str!("win_audio/dolby_atmos.rs")),
  ];

  #[test]
  fn every_tr_call_has_a_czech_translation() {
    let cs: HashMap<&str, &str> = crate::i18n_cs::CS.iter().copied().collect();
    let mut missing = Vec::new();
    for (file, src) in SOURCES {
      for key in tr_literals(src) {
        if !cs.contains_key(key.as_str()) {
          missing.push(format!("{file}: {key:?}"));
        }
      }
    }
    assert!(missing.is_empty(), "untranslated texts:\n{}", missing.join("\n"));
  }

  #[test]
  fn translations_are_unique_and_keep_placeholders() {
    let mut seen = std::collections::HashSet::new();
    for (en, cs) in crate::i18n_cs::CS {
      assert!(seen.insert(*en), "duplicate key {en:?}");
      assert_eq!(placeholders(en), placeholders(cs), "placeholders differ: {en:?} vs {cs:?}");
    }
  }

  #[test]
  fn fill_supports_sequential_and_indexed_placeholders() {
    assert_eq!(fill("{} of {}", &[&1, &3]), "1 of 3");
    assert_eq!(fill("{1} z {0}", &[&3, &1]), "1 z 3");
    assert_eq!(fill("{x} {", &[&1]), "{x} {");
    assert_eq!(fill("{} {}", &[&1]), "1 ");
  }

  #[test]
  fn unknown_language_preference_falls_back_to_system() {
    let pref: LanguagePref = serde_json::from_str("\"Klingon\"").unwrap();
    assert_eq!(pref, LanguagePref::System);
  }

  /// Number of argument slots a template uses (`{}` and `{N}` count alike).
  fn placeholders(s: &str) -> usize {
    let mut n = 0;
    let mut rest = s;
    while let Some(open) = rest.find('{') {
      let after = &rest[open + 1..];
      match after.find('}') {
        Some(close) if after[..close].chars().all(|c| c.is_ascii_digit()) => {
          n += 1;
          rest = &after[close + 1..];
        }
        _ => rest = after,
      }
    }
    n
  }

  /// The string literal values passed directly to `tr(` / `trf(` in a Rust source.
  fn tr_literals(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["tr(", "trf(", "tk("] {
      let mut rest = src;
      while let Some(pos) = rest.find(marker) {
        // Skip identifiers ending in "tr(" like "attr(".
        let prev = rest[..pos].chars().last();
        rest = &rest[pos + marker.len()..];
        if prev.is_some_and(|c| c.is_alphanumeric() || c == '_') {
          continue;
        }
        // The literal may start on the next line.
        let trimmed = rest.trim_start();
        let Some(body) = trimmed.strip_prefix('"') else {
          continue;
        };
        rest = body;
        if let Some((value, len)) = parse_literal(rest) {
          out.push(value);
          rest = &rest[len..];
        }
      }
    }
    out
  }

  /// Rust string literal body (after the opening quote) -> (value, bytes consumed).
  fn parse_literal(s: &str) -> Option<(String, usize)> {
    let mut value = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
      match c {
        '"' => return Some((value, i + 1)),
        '\\' => match chars.next()?.1 {
          'n' => value.push('\n'),
          't' => value.push('\t'),
          '\\' => value.push('\\'),
          '"' => value.push('"'),
          '\'' => value.push('\''),
          '\n' | '\r' => {
            while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
              chars.next();
            }
          }
          other => {
            value.push('\\');
            value.push(other);
          }
        },
        c => value.push(c),
      }
    }
    None
  }
}
