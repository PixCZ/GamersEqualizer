//! Plain-text share format of an EQ profile (any of the five EQs): Equalizer APO filter lines
//! with the name in a comment, so it also works in Equalizer APO. Copied into a chat or
//! pastebin and pasted back in with "Import…".

use super::autoeq::parse_parametric_eq;
use crate::i18n::trf;
use gamersequalizer_common::profile::{
  BiquadBand, FilterType, ParametricProfile, collapse_slopes, expand_slopes, slots_used,
};

pub const SHARE_HEADER: &str = "# GamersEqualizer EQ preset";
pub const SHARE_NAME_PREFIX: &str = "# Name:";

/// Parsed share text.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedPreset {
  pub name: Option<String>,
  pub bands: Vec<BiquadBand>,
  /// Things the user should know (lines skipped, bands dropped).
  pub notes: Vec<String>,
}

/// Equalizer APO text of the enabled bands, with the name in a comment line.
pub fn to_share_text(profile: &ParametricProfile) -> String {
  let mut text = format!("{SHARE_HEADER}\n{SHARE_NAME_PREFIX} {}\n", profile.name.replace(['\r', '\n'], " "));
  // Switched-off bands are not shared: the text is meant to be the sound you hear.
  // Steep cuts go out as their stacked biquads: plain Equalizer APO has no steepness.
  for (i, b) in expand_slopes(&profile.bands).iter().filter(|b| b.enabled).enumerate() {
    let n = i + 1;
    let (f, q) = (b.frequency, b.q);
    let line = match &b.filter_type {
      // Plain Equalizer APO has no line for a filter from a newer version of this app.
      FilterType::Unknown(_) => continue,
      FilterType::HighPass => format!("Filter {n}: ON HPQ Fc {f:.1} Hz Q {q:.3}"),
      FilterType::LowPass => format!("Filter {n}: ON LPQ Fc {f:.1} Hz Q {q:.3}"),
      FilterType::Notch => format!("Filter {n}: ON NO Fc {f:.1} Hz Q {q:.3}"),
      t => format!("Filter {n}: ON {} Fc {f:.1} Hz Gain {:.1} dB Q {q:.3}", t.apo_abbreviation(), b.gain_db),
    };
    text.push_str(&line);
    text.push('\n');
  }
  text
}

/// Parses share text (or plain Equalizer APO / AutoEQ lines); keeps at most `max_slots`
/// engine slots, the EQ it goes into.
pub fn from_share_text(text: &str, max_slots: usize) -> Result<SharedPreset, String> {
  let name = text
    .lines()
    .map(str::trim)
    .find_map(|l| l.get(..SHARE_NAME_PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(SHARE_NAME_PREFIX)).map(|_| l))
    .and_then(|l| l.get(SHARE_NAME_PREFIX.len()..))
    .map(|n| n.trim().to_string())
    .filter(|n| !n.is_empty());
  let parsed = parse_parametric_eq(text)?;
  let mut notes = Vec::new();
  if parsed.skipped > 0 {
    notes.push(trf("{} unrecognized lines were skipped.", &[&parsed.skipped]));
  }
  let mut bands = collapse_slopes(&parsed.bands);
  if slots_used(&bands) > max_slots {
    notes.push(trf("Too many filters (maximum is {}) — extra filters at the end were dropped.", &[&max_slots]));
    while slots_used(&bands) > max_slots {
      bands.pop();
    }
  }
  Ok(SharedPreset { name, bands, notes })
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::biquad::MAX_EQ_BANDS;
  use gamersequalizer_common::profile::VadType;

  fn band(filter_type: FilterType, frequency: f32, gain_db: f32, q: f32, slope_db: u8) -> BiquadBand {
    BiquadBand { enabled: true, filter_type, frequency, gain_db, q, slope_db }
  }

  fn radio() -> ParametricProfile {
    let mut p = ParametricProfile::new("Jirka: radio", VadType::Mic);
    p.bands = vec![
      band(FilterType::HighPass, 70.0, 0.0, 0.707, 48),
      band(FilterType::LowShelf, 150.0, 4.0, 0.707, 12),
      band(FilterType::Peaking, 450.0, -3.0, 1.0, 12),
      band(FilterType::LowPass, 14000.0, 0.0, 0.707, 24),
    ];
    p
  }

  #[test]
  fn share_text_round_trips_name_and_enabled_bands() {
    let p = radio();
    let shared = from_share_text(&to_share_text(&p), MAX_EQ_BANDS).unwrap();
    assert_eq!(shared.name.as_deref(), Some("Jirka: radio"));
    assert_eq!(shared.bands, collapse_slopes(&shared.bands), "steep cuts come back as one band");
    assert_eq!(shared.bands.len(), p.bands.len());
    assert!(shared.bands.iter().zip(&p.bands).all(|(a, b)| a.slope_db == b.slope_db));
    for (a, b) in shared.bands.iter().zip(&p.bands) {
      assert_eq!(a.filter_type, b.filter_type);
      assert!((a.frequency - b.frequency).abs() < 0.1);
      assert!(a.is_cut() || (a.q - b.q).abs() < 0.001);
      assert!((a.gain_db - b.gain_db).abs() < 0.05);
    }
  }

  #[test]
  fn share_text_skips_switched_off_bands_and_takes_plain_apo_text() {
    let mut p = radio();
    p.bands[2].enabled = false;
    assert_eq!(from_share_text(&to_share_text(&p), MAX_EQ_BANDS).unwrap().bands.len(), p.bands.len() - 1);

    let plain =
      from_share_text("Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1\nFilter 2: ON XYZ Fc 1 Hz", MAX_EQ_BANDS).unwrap();
    assert_eq!(plain.name, None);
    assert_eq!(plain.bands.len(), 1);
    assert_eq!(plain.notes.len(), 1);
    assert!(from_share_text("hello", MAX_EQ_BANDS).is_err());
  }
}
