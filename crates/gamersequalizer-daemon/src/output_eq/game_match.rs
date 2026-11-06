//! Which game profile belongs to a foreground process. Pure logic over facts gathered by
//! `launchers` (so it is testable without Steam installed). Order of precedence:
//! 1. the user's binding for this exe (or "not a game"),
//! 2. a launcher id the Sonar profile lists (`1_<steam appid>`, `8_<ubisoft id>`, ...),
//! 3. names (launcher title, exe product name, exe name) matched against profile names,
//! 4. a launcher says it is a game, but no profile fits: detected without a profile.

use super::settings::OutputEqSettings;
use gamersequalizer_common::profile::ParametricProfile;
use std::collections::BTreeSet;

/// Ignored when comparing names: articles, publisher prefixes, build/edition noise.
const STOPWORDS: [&str; 24] = [
  "the",
  "of",
  "a",
  "an",
  "and",
  "tom",
  "clancys",
  "edition",
  "game",
  "games",
  "launcher",
  "client",
  "shipping",
  "win64",
  "win32",
  "x64",
  "x86",
  "dx11",
  "dx12",
  "vulkan",
  "pro",
  "preset",
  "remastered",
  "definitive",
];
/// A one-word profile name must be at least this long to match by name (avoids "AION" vs junk).
const MIN_SINGLE_TOKEN_LEN: usize = 4;
/// Compact (spaces removed) containment needs this many characters ("eldenring" in "eldenringlauncher").
const MIN_COMPACT_LEN: usize = 6;

/// Everything known about one executable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GameFacts {
  /// Lowercase file name ("division2.exe").
  pub exe_name: String,
  /// Launcher ids in Sonar's format ("1_2221490").
  pub launcher_ids: Vec<String>,
  /// Titles from launchers and the exe's version info ("Tom Clancy's The Division 2").
  pub names: Vec<String>,
  /// Installed through a game launcher: a game even if no profile matches.
  pub from_launcher: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMethod {
  Binding,
  LauncherId,
  Name,
  /// A launcher game without a profile.
  NoProfile,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GameMatch {
  pub profile: Option<String>,
  /// Shown in the UI: "Tom Clancy's The Division 2 (division2.exe)".
  pub label: String,
  pub method: MatchMethod,
}

pub fn resolve(settings: &OutputEqSettings, profiles: &[ParametricProfile], facts: &GameFacts) -> Option<GameMatch> {
  let label = match facts.names.first() {
    Some(title) => format!("{title} ({})", facts.exe_name),
    None => facts.exe_name.clone(),
  };
  let found = |profile: &ParametricProfile, method| {
    Some(GameMatch { profile: Some(profile.name.clone()), label: label.clone(), method })
  };

  if let Some(bound) = settings.bindings.get(&facts.exe_name) {
    if bound.is_empty() {
      return None; // marked "not a game"
    }
    if let Some(p) = profiles.iter().find(|p| &p.name == bound) {
      return found(p, MatchMethod::Binding);
    }
  }
  if !settings.auto_detect {
    return None;
  }
  if settings.detect_launchers {
    for id in &facts.launcher_ids {
      if let Some(p) = profiles.iter().find(|p| p.game_ids.iter().any(|g| g.eq_ignore_ascii_case(id))) {
        return found(p, MatchMethod::LauncherId);
      }
    }
  }
  if settings.detect_names {
    if let Some(p) = best_name_match(profiles, facts) {
      return found(p, MatchMethod::Name);
    }
  }
  facts.from_launcher.then_some(GameMatch { profile: None, label, method: MatchMethod::NoProfile })
}

/// Words of a name: camelCase and letter/digit boundaries split ("Division2" -> division 2),
/// apostrophes dropped ("Clancy's" -> clancys), roman numerals II..IX as digits, stopwords out.
pub fn tokenize(name: &str) -> Vec<String> {
  let mut words: Vec<String> = Vec::new();
  let mut current = String::new();
  let mut prev: Option<char> = None;
  for c in name.chars() {
    if c == '\'' || c == '\u{2019}' {
      continue;
    }
    if !c.is_alphanumeric() {
      flush(&mut current, &mut words);
      prev = None;
      continue;
    }
    if let Some(p) = prev {
      let boundary = (p.is_lowercase() && c.is_uppercase()) || (p.is_alphabetic() != c.is_alphabetic());
      if boundary {
        flush(&mut current, &mut words);
      }
    }
    current.extend(c.to_lowercase());
    prev = Some(c);
  }
  flush(&mut current, &mut words);
  words
    .into_iter()
    .map(|w| roman_to_digit(&w).map(str::to_string).unwrap_or(w))
    .filter(|w| !STOPWORDS.contains(&w.as_str()))
    .collect()
}

/// One line for the log: how `resolve` classified this app and from which facts.
pub fn explain(settings: &OutputEqSettings, facts: &GameFacts, matched: Option<&GameMatch>) -> String {
  let ids = if facts.launcher_ids.is_empty() { "none".to_string() } else { facts.launcher_ids.join(", ") };
  let names = if facts.names.is_empty() { "none".to_string() } else { facts.names.join(" / ") };
  let seen = format!("launcher ids: {ids}; names: {names}");
  let profile = |m: &GameMatch| m.profile.clone().unwrap_or_default();
  match matched {
    Some(m) => match m.method {
      MatchMethod::Binding => format!("game {}, assigned by you to profile {}", m.label, profile(m)),
      MatchMethod::LauncherId => format!("game {}, profile {} by launcher id ({seen})", m.label, profile(m)),
      MatchMethod::Name => format!("game {}, profile {} by name ({seen})", m.label, profile(m)),
      MatchMethod::NoProfile => format!("game {} (installed by a launcher), no profile matches ({seen})", m.label),
    },
    None if settings.bindings.get(&facts.exe_name).is_some_and(String::is_empty) => {
      "marked by you as not a game".to_string()
    }
    None if !settings.auto_detect => "automatic detection is off".to_string(),
    None => format!("not a game ({seen})"),
  }
}

// --- private helpers ---------------------------------------------------------------------

fn flush(current: &mut String, words: &mut Vec<String>) {
  if !current.is_empty() {
    words.push(std::mem::take(current));
  }
}

/// "I", "V" and "X" stay words: too common as letters ("Rainbow Six" is spelled out anyway).
fn roman_to_digit(w: &str) -> Option<&'static str> {
  Some(match w {
    "ii" => "2",
    "iii" => "3",
    "iv" => "4",
    "vi" => "6",
    "vii" => "7",
    "viii" => "8",
    "ix" => "9",
    _ => return None,
  })
}

/// Profile name without Sonar's suffixes ("Rainbow Six Siege by Ubisoft", "CS2 Pro Preset").
fn profile_tokens(name: &str) -> Vec<String> {
  let mut tokens = tokenize(name);
  if let Some(by) = tokens.iter().position(|t| t == "by") {
    if by > 0 {
      tokens.truncate(by);
    }
  }
  tokens
}

/// Name-like Sonar ids ("4_Destiny 2", "-1_Genshin Impact game") count as extra profile names;
/// numeric ids, GUIDs, hex ids and package names do not.
fn alias_names(profile: &ParametricProfile) -> impl Iterator<Item = &str> {
  profile.game_ids.iter().filter_map(|id| {
    let (_, value) = id.split_once('_')?;
    let hexish = value.chars().all(|c| c.is_ascii_hexdigit());
    let looks_like_name = value.chars().any(char::is_alphabetic) && !hexish && !value.contains(['{', '.']);
    looks_like_name.then_some(value)
  })
}

fn best_name_match<'a>(profiles: &'a [ParametricProfile], facts: &GameFacts) -> Option<&'a ParametricProfile> {
  let exe_stem = facts.exe_name.strip_suffix(".exe").unwrap_or(&facts.exe_name);
  let candidates: Vec<Vec<String>> = facts
    .names
    .iter()
    .map(String::as_str)
    .chain(std::iter::once(exe_stem))
    .map(tokenize)
    .filter(|t| !t.is_empty())
    .collect();

  let mut best: Option<(usize, &ParametricProfile)> = None;
  for profile in profiles {
    let names = std::iter::once(profile_tokens(&profile.name)).chain(alias_names(profile).map(profile_tokens));
    for wanted in names {
      for candidate in &candidates {
        if let Some(score) = name_score(&wanted, candidate) {
          if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, profile));
          }
        }
      }
    }
  }
  best.map(|(_, p)| p)
}

/// Higher is better; None = no match. Every profile word must appear in the candidate, or the
/// words glued together must equal / be contained in the candidate's ("eldenring.exe").
fn name_score(wanted: &[String], candidate: &[String]) -> Option<usize> {
  let first_len = wanted.first().map_or(0, String::len);
  if wanted.is_empty() || (wanted.len() == 1 && first_len < MIN_SINGLE_TOKEN_LEN) {
    return None;
  }
  let have: BTreeSet<&str> = candidate.iter().map(String::as_str).collect();
  // More matched words win; among equals, the candidate with fewer extra words.
  let extra = candidate.len().saturating_sub(wanted.len()).min(9);
  if wanted.iter().all(|w| have.contains(w.as_str())) {
    return Some(wanted.len() * 10 + (9 - extra));
  }
  let (w, c) = (wanted.concat(), candidate.concat());
  (w.len() >= MIN_COMPACT_LEN && c.contains(&w)).then_some(wanted.len() * 10)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::VadType;

  fn profile(name: &str, ids: &[&str]) -> ParametricProfile {
    let mut p = ParametricProfile::new(name, VadType::Game);
    p.game_ids = ids.iter().map(|s| s.to_string()).collect();
    p
  }

  fn profiles() -> Vec<ParametricProfile> {
    vec![
      profile("The Division 2", &["1_2221490", "8_4932"]),
      profile("The Division", &[]),
      profile("CS2 Pro Preset", &["1_730"]),
      profile("Rainbow Six Siege by Ubisoft", &["8_635"]),
      profile("Elden Ring", &[]),
      profile("Genshin Impact", &["-1_Genshin Impact game"]),
      profile("AION", &[]),
    ]
  }

  fn facts(exe: &str, ids: &[&str], names: &[&str]) -> GameFacts {
    GameFacts {
      exe_name: exe.to_string(),
      launcher_ids: ids.iter().map(|s| s.to_string()).collect(),
      names: names.iter().map(|s| s.to_string()).collect(),
      from_launcher: !ids.is_empty(),
    }
  }

  fn matched(f: &GameFacts) -> Option<String> {
    resolve(&OutputEqSettings::default(), &profiles(), f).and_then(|m| m.profile)
  }

  #[test]
  fn division2_is_found_by_steam_id_product_name_or_exe_name() {
    // Steam manifest.
    let by_id = resolve(
      &OutputEqSettings::default(),
      &profiles(),
      &facts("division2.exe", &["1_2221490"], &["Tom Clancy's The Division 2"]),
    )
    .unwrap();
    assert_eq!(by_id.profile.as_deref(), Some("The Division 2"));
    assert_eq!(by_id.method, MatchMethod::LauncherId);
    assert_eq!(by_id.label, "Tom Clancy's The Division 2 (division2.exe)");
    // Exe version info only (e.g. a non-Steam install).
    assert_eq!(
      matched(&facts("division2.exe", &[], &["Tom Clancy's The Division® 2"])).as_deref(),
      Some("The Division 2")
    );
    // Nothing but the exe name.
    assert_eq!(matched(&facts("division2.exe", &[], &[])).as_deref(), Some("The Division 2"));
    // The first game is not mistaken for the sequel.
    assert_eq!(
      matched(&facts("thedivision.exe", &[], &["Tom Clancy's The Division"])).as_deref(),
      Some("The Division")
    );
  }

  #[test]
  fn suffixes_glued_names_aliases_and_ubisoft_ids() {
    assert_eq!(matched(&facts("cs2.exe", &[], &[])).as_deref(), Some("CS2 Pro Preset"));
    assert_eq!(matched(&facts("eldenring.exe", &[], &[])).as_deref(), Some("Elden Ring"));
    assert_eq!(matched(&facts("GenshinImpact.exe", &[], &[])).as_deref(), Some("Genshin Impact"));
    assert_eq!(matched(&facts("rainbowsix.exe", &["8_635"], &[])).as_deref(), Some("Rainbow Six Siege by Ubisoft"));
  }

  #[test]
  fn non_games_and_short_names_do_not_match() {
    assert_eq!(resolve(&OutputEqSettings::default(), &profiles(), &facts("chrome.exe", &[], &["Google Chrome"])), None);
    assert_eq!(matched(&facts("ai.exe", &[], &[])), None);
    // A launcher game without a profile is still a game.
    let m = resolve(&OutputEqSettings::default(), &profiles(), &facts("foo.exe", &["1_99"], &["Foo"])).unwrap();
    assert_eq!((m.profile, m.method), (None, MatchMethod::NoProfile));
  }

  #[test]
  fn bindings_and_switches_take_precedence() {
    let mut s = OutputEqSettings::default();
    s.bindings.insert("weird.exe".into(), "Elden Ring".into());
    s.bindings.insert("division2.exe".into(), String::new());
    let r = resolve(&s, &profiles(), &facts("weird.exe", &[], &[])).unwrap();
    assert_eq!((r.profile.as_deref(), r.method), (Some("Elden Ring"), MatchMethod::Binding));
    assert_eq!(resolve(&s, &profiles(), &facts("division2.exe", &["1_2221490"], &[])), None);

    let off = OutputEqSettings { auto_detect: false, ..Default::default() };
    assert_eq!(resolve(&off, &profiles(), &facts("cs2.exe", &["1_730"], &[])), None);
    let no_launchers = OutputEqSettings { detect_launchers: false, detect_names: false, ..Default::default() };
    assert_eq!(resolve(&no_launchers, &profiles(), &facts("x.exe", &["1_730"], &[])).and_then(|m| m.profile), None);
  }

  #[test]
  fn tokenizer_splits_camel_case_digits_and_roman_numerals() {
    assert_eq!(tokenize("Tom Clancy's The Division® 2"), vec!["division", "2"]);
    assert_eq!(tokenize("Division2"), vec!["division", "2"]);
    assert_eq!(tokenize("COD Modern Warfare II"), vec!["cod", "modern", "warfare", "2"]);
    assert_eq!(tokenize("r5apex"), vec!["r", "5", "apex"]);
  }
}
