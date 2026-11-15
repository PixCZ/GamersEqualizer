//! AutoEQ (github.com/jaakkopasanen/AutoEq) headphone corrections, fully offline.
//! The app never downloads anything: the user pastes / imports a `ParametricEQ.txt`, or points
//! the app at a downloaded copy of the AutoEq repository, which is then indexed and searched.

use super::auto_preamp::{evaluate_bands_response, generate_log_frequencies};
use crate::i18n::{tr, trf};
use gamersequalizer_common::biquad::MAX_OUT_EQ_BANDS;
use gamersequalizer_common::profile::{BiquadBand, FilterType};
use std::path::{Path, PathBuf};

/// AutoEQ result files are named "<Headphone> ParametricEQ.txt".
const PARAMETRIC_SUFFIX: &str = " ParametricEQ.txt";
/// The AutoEq tree is ~20k files; deeper is not AutoEq.
const MAX_SCAN_DEPTH: usize = 8;
const MAX_RESULTS: usize = 60;
/// More headphone bands than this leaves little room for the game profile (32 in total).
const MAX_HEADPHONE_BANDS: usize = 16;
/// AutoEQ rarely goes beyond this; more usually means a wrong / edited file.
const STRONG_GAIN_DB: f32 = 12.0;
/// The file's preamp should be about -(max boost); a bigger gap means the file was edited.
const PREAMP_TOLERANCE_DB: f32 = 1.5;
/// Downloads files older than this are not offered as "just downloaded".
const RECENT_DOWNLOAD: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, PartialEq)]
pub struct AutoEqEntry {
  /// Headphone model ("Sennheiser HD 600").
  pub name: String,
  /// Measurement source + form factor ("oratory1990 / over-ear"), several per model.
  pub source: String,
  pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEq {
  pub bands: Vec<BiquadBand>,
  pub preamp_db: f32,
  /// Lines that looked like filters but could not be used (unknown type, too many).
  pub skipped: usize,
}

/// Equalizer APO / AutoEQ text: `Preamp: -6.2 dB`, `Filter 1: ON LSC Fc 105 Hz Gain 6.5 dB Q 0.70`.
pub fn parse_parametric_eq(text: &str) -> Result<ParsedEq, String> {
  let mut parsed = ParsedEq { bands: Vec::new(), preamp_db: 0.0, skipped: 0 };
  for line in text.lines().map(str::trim) {
    let lower = line.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("preamp:") {
      parsed.preamp_db = first_number(rest).unwrap_or(0.0);
      continue;
    }
    if !lower.starts_with("filter") {
      continue;
    }
    match parse_filter_line(line) {
      Some(Some(band)) if parsed.bands.len() < MAX_OUT_EQ_BANDS => parsed.bands.push(band),
      Some(None) => {} // "OFF" filter
      _ => parsed.skipped += 1,
    }
  }
  if parsed.bands.is_empty() {
    return Err(
      tr("no filters found (expected lines like \"Filter 1: ON PK Fc 100 Hz Gain -3.0 dB Q 1.41\")").to_string(),
    );
  }
  Ok(parsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
  Ok,
  Warning,
  /// The file can't be used.
  Error,
}

/// One line of the import check shown to the user, with what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
  pub level: CheckLevel,
  pub text: String,
}

/// Explains what is in the file and what may be wrong with it, in the user's terms.
/// `file_name` helps to spot the wrong AutoEQ variant (GraphicEQ, FixedBandEQ).
pub fn check_file(text: &str, file_name: &str) -> (Option<ParsedEq>, Vec<Check>) {
  let mut checks = Vec::new();
  let push = |checks: &mut Vec<Check>, level, text: String| checks.push(Check { level, text });
  let lower = text.to_lowercase();
  let lower_name = file_name.to_lowercase();
  if lower.contains("graphiceq:") || lower_name.contains("graphiceq") {
    let msg = tr("This is the GraphicEQ file (one long line of points). Download the ParametricEQ file instead: \"<model> ParametricEQ.txt\".");
    push(&mut checks, CheckLevel::Error, msg.to_string());
    return (None, checks);
  }
  if lower_name.contains("fixedbandeq") {
    let msg = tr("This is the FixedBandEQ variant (10 fixed bands, less precise). The ParametricEQ file fits better.");
    push(&mut checks, CheckLevel::Warning, msg.to_string());
  }
  let parsed = match parse_parametric_eq(text) {
    Ok(p) => p,
    Err(e) => {
      push(&mut checks, CheckLevel::Error, trf("No usable filters: {}. Is this the right file?", &[&e]));
      return (None, checks);
    }
  };

  let freqs = generate_log_frequencies();
  let gains = evaluate_bands_response(&parsed.bands, &freqs, 48_000.0);
  let max_boost = gains.iter().fold(0.0f32, |m, g| m.max(*g));
  let (lo, hi) = parsed.bands.iter().fold((f32::MAX, 0.0f32), |(lo, hi), b| (lo.min(b.frequency), hi.max(b.frequency)));
  push(
    &mut checks,
    CheckLevel::Ok,
    trf(
      "{} filters between {} Hz and {} Hz, strongest boost {} dB.",
      &[&parsed.bands.len(), &format!("{lo:.0}"), &format!("{hi:.0}"), &format!("{max_boost:+.1}")],
    ),
  );
  push(
    &mut checks,
    CheckLevel::Ok,
    trf(
      "The app lowers the volume by {} dB so boosts can't clip (auto preamp, as AutoEQ recommends).",
      &[&format!("{:.1}", max_boost + 0.5)],
    ),
  );
  if parsed.skipped > 0 {
    push(
      &mut checks,
      CheckLevel::Warning,
      trf("{} lines were not understood and are skipped (unknown filter type).", &[&parsed.skipped]),
    );
  }
  if parsed.bands.len() > MAX_HEADPHONE_BANDS {
    let msg = trf(
      "{} filters is a lot: together with a game profile only 32 fit, the rest is dropped.",
      &[&parsed.bands.len()],
    );
    push(&mut checks, CheckLevel::Warning, msg);
  }
  if let Some(b) = parsed.bands.iter().find(|b| b.filter_type.uses_gain() && b.gain_db.abs() > STRONG_GAIN_DB) {
    let msg = trf(
      "Unusually strong filter ({} dB at {} Hz). Check that the model is right.",
      &[&format!("{:+.1}", b.gain_db), &format!("{:.0}", b.frequency)],
    );
    push(&mut checks, CheckLevel::Warning, msg);
  }
  if parsed.preamp_db != 0.0 && (parsed.preamp_db + max_boost).abs() > PREAMP_TOLERANCE_DB {
    let msg = trf(
      "The file's preamp ({} dB) does not match its filters; it may have been edited. The app uses its own preamp.",
      &[&format!("{:.1}", parsed.preamp_db)],
    );
    push(&mut checks, CheckLevel::Warning, msg);
  }
  (Some(parsed), checks)
}

/// What the wizard found in the user's Downloads folder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DownloadsScan {
  /// `*ParametricEQ*.txt` files, newest first (downloaded in the last 24 h).
  pub parametric_files: Vec<PathBuf>,
  /// A wrong variant was downloaded (GraphicEQ / FixedBandEQ), newest first.
  pub other_variants: Vec<PathBuf>,
  /// `AutoEq*.zip` that still needs to be extracted.
  pub autoeq_zip: Option<PathBuf>,
}

pub fn downloads_dir() -> Option<PathBuf> {
  std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads"))
}

/// Looks at the top level of `dir` only (cheap enough to repeat every few seconds).
pub fn scan_downloads(dir: &Path) -> DownloadsScan {
  let mut found: Vec<(std::time::SystemTime, PathBuf, String)> = Vec::new();
  if let Ok(entries) = std::fs::read_dir(dir) {
    for entry in entries.flatten() {
      let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
        continue;
      };
      let name = entry.file_name().to_string_lossy().to_lowercase();
      found.push((modified, entry.path(), name));
    }
  }
  found.sort_by_key(|f| std::cmp::Reverse(f.0));
  let now = std::time::SystemTime::now();
  let recent = |t: &std::time::SystemTime| now.duration_since(*t).map_or(true, |age| age <= RECENT_DOWNLOAD);
  let mut scan = DownloadsScan::default();
  for (modified, path, name) in found {
    if name.ends_with(".zip") && name.starts_with("autoeq") {
      scan.autoeq_zip.get_or_insert(path);
    } else if name.ends_with(".txt") && recent(&modified) {
      if name.contains("parametriceq") {
        scan.parametric_files.push(path);
      } else if name.contains("graphiceq") || name.contains("fixedbandeq") {
        scan.other_variants.push(path);
      }
    }
  }
  scan
}

/// Recursively finds `* ParametricEQ.txt` under `dir`. Unreadable folders are skipped.
pub fn index_folder(dir: &Path) -> Vec<AutoEqEntry> {
  let mut out = Vec::new();
  scan(dir, dir, 0, &mut out);
  out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.source.cmp(&b.source)));
  out
}

/// All query words must appear in the name (case-insensitive); the output device name works
/// well as a query ("Rode NTH-100").
pub fn search<'a>(entries: &'a [AutoEqEntry], query: &str) -> Vec<&'a AutoEqEntry> {
  let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
  entries
    .iter()
    .filter(|e| {
      let name = e.name.to_lowercase();
      words.iter().all(|w| name.contains(w.as_str()))
    })
    .take(MAX_RESULTS)
    .collect()
}

/// Likely places of a downloaded AutoEq copy: the configured folder first.
pub fn candidate_dirs(configured: Option<&Path>) -> Vec<PathBuf> {
  let mut dirs: Vec<PathBuf> = configured.map(Path::to_path_buf).into_iter().collect();
  if let Some(app) = std::env::var_os("APPDATA") {
    dirs.push(PathBuf::from(app).join("GamersEqualizer").join("AutoEq"));
  }
  if let Some(home) = std::env::var_os("USERPROFILE") {
    let downloads = PathBuf::from(home).join("Downloads");
    for name in ["AutoEq-master", "AutoEq-main", "AutoEq"] {
      dirs.push(downloads.join(name));
    }
  }
  // Prefer the `results` subfolder of a repository checkout.
  dirs
    .into_iter()
    .map(|d| if d.join("results").is_dir() { d.join("results") } else { d })
    .filter(|d| d.is_dir())
    .collect()
}

// --- private helpers ---------------------------------------------------------------------

/// Some(Some(band)) = usable, Some(None) = filter switched OFF, None = not understood.
fn parse_filter_line(line: &str) -> Option<Option<BiquadBand>> {
  let (_, body) = line.split_once(':')?;
  let tokens: Vec<&str> = body.split_whitespace().collect();
  let mut it = tokens.iter().map(|t| t.to_ascii_uppercase());
  let state = it.next()?;
  if state == "OFF" {
    return Some(None);
  }
  if state != "ON" {
    return None;
  }
  let filter_type = match it.next()?.as_str() {
    "PK" | "PEQ" | "MODAL" => FilterType::Peaking,
    "LSC" | "LS" | "LSQ" => FilterType::LowShelf,
    "HSC" | "HS" | "HSQ" => FilterType::HighShelf,
    "LP" | "LPQ" => FilterType::LowPass,
    "HP" | "HPQ" => FilterType::HighPass,
    "NO" => FilterType::Notch,
    _ => return None,
  };
  let value_after = |key: &str| -> Option<f32> {
    let pos = tokens.iter().position(|t| t.eq_ignore_ascii_case(key))?;
    tokens.get(pos + 1)?.replace(',', ".").parse::<f32>().ok().filter(|v| v.is_finite())
  };
  let mut band = BiquadBand {
    enabled: true,
    filter_type,
    frequency: value_after("Fc")?,
    gain_db: value_after("Gain").unwrap_or(0.0),
    // Shelves without Q in Equalizer APO default to a 12 dB/oct-ish slope (Q 0.707).
    q: value_after("Q").unwrap_or(0.707),
    slope_db: 12,
  };
  band.clamp_parameters();
  Some(Some(band))
}

fn first_number(s: &str) -> Option<f32> {
  s.split_whitespace().find_map(|t| t.replace(',', ".").parse::<f32>().ok()).filter(|v| v.is_finite())
}

fn scan(root: &Path, dir: &Path, depth: usize, out: &mut Vec<AutoEqEntry>) {
  if depth > MAX_SCAN_DEPTH {
    return;
  }
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(kind) = entry.file_type() else {
      continue;
    };
    if kind.is_dir() {
      scan(root, &path, depth + 1, out);
      continue;
    }
    let file_name = entry.file_name().to_string_lossy().into_owned();
    let Some(name) = file_name.strip_suffix(PARAMETRIC_SUFFIX) else {
      continue;
    };
    // results/<source>/<form>/<model>/<model> ParametricEQ.txt -> "<source> / <form>"
    let source = path
      .parent()
      .and_then(Path::parent)
      .and_then(|p| p.strip_prefix(root).ok())
      .map(|rel| rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join(" / "))
      .unwrap_or_default();
    out.push(AutoEqEntry { name: name.to_string(), source, path });
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
  use super::*;

  const SAMPLE: &str = "Preamp: -6.4 dB\r\n\
Filter 1: ON LSC Fc 105 Hz Gain 6.7 dB Q 0.70\r\n\
Filter 2: ON PK Fc 3170 Hz Gain -3.1 dB Q 2.13\r\n\
Filter 3: OFF PK Fc 100 Hz Gain 1.0 dB Q 1.00\r\n\
Filter 4: ON HSC Fc 10000 Hz Gain -2.5 dB Q 0.70\r\n\
Filter 5: ON XYZ Fc 10 Hz\r\n";

  #[test]
  fn parses_autoeq_parametric_file() {
    let eq = parse_parametric_eq(SAMPLE).unwrap();
    assert_eq!(eq.preamp_db, -6.4);
    assert_eq!(eq.bands.len(), 3);
    assert_eq!(eq.skipped, 1);
    assert_eq!(
      eq.bands[0],
      BiquadBand {
        enabled: true,
        filter_type: FilterType::LowShelf,
        frequency: 105.0,
        gain_db: 6.7,
        q: 0.70,
        slope_db: 12
      }
    );
    assert_eq!(eq.bands[2].filter_type, FilterType::HighShelf);
  }

  #[test]
  fn equalizer_apo_style_and_garbage() {
    let eq = parse_parametric_eq("Filter: ON PK Fc 1000 Hz Gain 3,5 dB Q 1.41").unwrap();
    assert_eq!(eq.bands[0].gain_db, 3.5);
    assert!(parse_parametric_eq("hello\nworld").is_err());
    assert!(parse_parametric_eq("Filter 1: ON PK Fc NaN Hz Gain 1 dB Q 1").is_err());
  }

  #[test]
  fn check_explains_good_wrong_and_suspicious_files() {
    let (parsed, checks) = check_file(SAMPLE, "HD 600 ParametricEQ.txt");
    assert!(parsed.is_some());
    assert!(checks.iter().all(|c| c.level != CheckLevel::Error));
    assert!(checks.iter().any(|c| c.text.contains("3 filters")));
    assert!(checks.iter().any(|c| c.text.contains("not understood")), "the XYZ line is reported");

    let (parsed, checks) = check_file("GraphicEQ: 20 -1.2; 21 -1.1", "HD 600 GraphicEQ.txt");
    assert!(parsed.is_none());
    assert!(checks.iter().any(|c| c.level == CheckLevel::Error && c.text.contains("ParametricEQ")));

    let (_, checks) = check_file("Preamp: -1 dB\nFilter 1: ON PK Fc 1000 Hz Gain 15 dB Q 1", "x ParametricEQ.txt");
    assert!(checks.iter().any(|c| c.text.contains("Unusually strong")));
    assert!(checks.iter().any(|c| c.text.contains("does not match")));

    let (parsed, checks) = check_file("hello", "notes.txt");
    assert!(parsed.is_none() && checks.iter().any(|c| c.level == CheckLevel::Error));
  }

  #[test]
  fn downloads_scan_finds_files_variants_and_zip() {
    let dir = std::env::temp_dir().join(format!("ge_downloads_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Sennheiser HD 600 ParametricEQ.txt"), SAMPLE).unwrap();
    std::fs::write(dir.join("Sennheiser HD 600 GraphicEQ.txt"), "GraphicEQ: 20 0").unwrap();
    std::fs::write(dir.join("AutoEq-master.zip"), "zip").unwrap();
    std::fs::write(dir.join("invoice.txt"), "x").unwrap();
    let scan = scan_downloads(&dir);
    assert_eq!(scan.parametric_files.len(), 1);
    assert_eq!(scan.other_variants.len(), 1);
    assert!(scan.autoeq_zip.is_some());
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn indexes_and_searches_a_results_tree() {
    let root = std::env::temp_dir().join(format!("ge_autoeq_test_{}", std::process::id()));
    let model = root.join("oratory1990").join("over-ear").join("Sennheiser HD 600");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("Sennheiser HD 600 ParametricEQ.txt"), SAMPLE).unwrap();
    std::fs::write(model.join("Sennheiser HD 600 GraphicEQ.txt"), "x").unwrap();

    let entries = index_folder(&root);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "Sennheiser HD 600");
    assert_eq!(entries[0].source, "oratory1990 / over-ear");
    assert_eq!(search(&entries, "hd 600").len(), 1);
    assert!(search(&entries, "hd 650").is_empty());
    let _ = std::fs::remove_dir_all(&root);
  }
}
