use gamersequalizer_common::profile::{BiquadBand, FilterType, ParametricProfile, VadType};
use serde_json::Value;

pub fn map_sonar_filter_type(sonar_type: &str) -> FilterType {
  match sonar_type {
    "peakingEQ" => FilterType::Peaking,
    "lowShelving" => FilterType::LowShelf,
    "highShelving" => FilterType::HighShelf,
    "lowPass" => FilterType::LowPass,
    "highPass" => FilterType::HighPass,
    "notchFilter" | "notch" => FilterType::Notch,
    _ => FilterType::Peaking,
  }
}

/// Sonar's `configs.vad` numbering (game, chatRender, chatCapture, media, aux). It differs
/// from `VadType`'s ids: 3 is the microphone, 4 is media.
pub fn map_sonar_vad(vad: u32) -> VadType {
  match vad {
    2 => VadType::Chat,
    3 => VadType::Mic,
    4 => VadType::Media,
    5 => VadType::Aux,
    _ => VadType::Game,
  }
}

pub fn convert_sonar_config(
  name: &str,
  vad: u32,
  is_preset: bool,
  raw_data_json: &str,
) -> Result<ParametricProfile, String> {
  let vad_type = map_sonar_vad(vad);
  // GG's preset scripts write JavaScript object literals; Sonar's database holds JSON.
  let v: Value = serde_json::from_str(raw_data_json)
    .or_else(|_| serde_json::from_str(&js_object_to_json(raw_data_json)))
    .map_err(|e| format!("Invalid JSON in Sonar DB: {}", e))?;

  let mut profile = ParametricProfile::new(name, vad_type);
  profile.is_custom = !is_preset;

  let peq = match v.get("parametricEQ") {
    Some(Value::Object(map)) => map,
    _ => return Ok(profile),
  };

  // Find all keys starting with "filter"
  let mut filter_entries: Vec<(usize, &Value)> = Vec::new();
  for (k, val) in peq.iter() {
    if k.starts_with("filter") {
      let idx_str = &k["filter".len()..];
      if let Ok(idx) = idx_str.parse::<usize>() {
        filter_entries.push((idx, val));
      }
    }
  }

  // Sort filters by index (e.g. filter1, filter2, ...)
  filter_entries.sort_by_key(|&(idx, _)| idx);

  for (_, filter_val) in filter_entries {
    let enabled = filter_val.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    let type_str = filter_val.get("type").and_then(|v| v.as_str()).unwrap_or("peakingEQ");
    let filter_type = map_sonar_filter_type(type_str);

    let freq = filter_val.get("frequency").and_then(|v| v.as_f64()).map(|v| v as f32).unwrap_or(1000.0);

    let gain = filter_val.get("gain").and_then(|v| v.as_f64()).map(|v| v as f32).unwrap_or(0.0);

    let q = filter_val.get("qFactor").and_then(|v| v.as_f64()).map(|v| v as f32).unwrap_or(1.414);

    let mut band = BiquadBand { enabled, filter_type, frequency: freq, gain_db: gain, q, slope_db: 12 };
    band.clamp_parameters();
    profile.bands.push(band);
  }

  // Preamp / generalGain
  if let Some(gain) = v.get("generalGain").and_then(|v| v.as_f64()) {
    profile.preamp_db = gain as f32;
  }

  Ok(profile)
}

/// `{ key: 1, list: [2,], }` -> `{ "key": 1, "list": [2] }`: quotes bare keys and drops trailing
/// commas outside strings. Only what GG's scripts use (no comments, no single quotes).
fn js_object_to_json(js: &str) -> String {
  let chars: Vec<char> = js.chars().collect();
  let mut out = String::with_capacity(js.len() + js.len() / 4);
  let mut i = 0;
  while let Some(&c) = chars.get(i) {
    if c == '"' {
      // Copy the string as is, escapes included.
      out.push(c);
      i += 1;
      while let Some(&s) = chars.get(i) {
        out.push(s);
        i += 1;
        if s == '\\' {
          if let Some(&next) = chars.get(i) {
            out.push(next);
            i += 1;
          }
        } else if s == '"' {
          break;
        }
      }
      continue;
    }
    let next_solid = |from: usize| chars.iter().skip(from).copied().find(|c| !c.is_whitespace());
    if c.is_ascii_alphabetic() || c == '_' || c == '$' {
      let start = i;
      while chars.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$') {
        i += 1;
      }
      let word: String = chars.get(start..i).unwrap_or_default().iter().collect();
      if next_solid(i) == Some(':') {
        out.push('"');
        out.push_str(&word);
        out.push('"');
      } else {
        out.push_str(&word); // true, false, null
      }
      continue;
    }
    if !(c == ',' && matches!(next_solid(i + 1), Some('}' | ']'))) {
      out.push(c);
    }
    i += 1;
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_convert_sonar_config() {
    let json_data = r#"{
      "generalGain": -2.0,
      "parametricEQ": {
        "enabled": true,
        "filter1": {
          "enabled": true,
          "frequency": 80.0,
          "gain": 3.0,
          "qFactor": 1.414,
          "type": "lowShelving"
        },
        "filter2": {
          "enabled": true,
          "frequency": 3500.0,
          "gain": 5.5,
          "qFactor": 2.0,
          "type": "peakingEQ"
        }
      }
    }"#;

    let profile = convert_sonar_config("Valorant Pro", 1, true, json_data).unwrap();
    assert_eq!(profile.name, "Valorant Pro");
    assert_eq!(profile.vad_type, VadType::Game);
    assert_eq!(profile.bands.len(), 2);
    assert_eq!(profile.bands[0].filter_type, FilterType::LowShelf);
    assert_eq!(profile.bands[0].frequency, 80.0);
    assert_eq!(profile.bands[1].filter_type, FilterType::Peaking);
    assert_eq!(profile.bands[1].frequency, 3500.0);
  }

  #[test]
  fn gg_script_object_literals_convert() {
    let js = r#"{
  generalGain: -1.5,
  note: "a: b, }\" x",
  parametricEQ: {
    enabled: true,
    filter1: { enabled: true, qFactor: 0.707, frequency: 80, gain: 0, type: "highPass", },
  },
}"#;
    let profile = convert_sonar_config("PUBG", 1, true, js).unwrap();
    assert_eq!((profile.preamp_db, profile.bands.len()), (-1.5, 1));
    assert_eq!(profile.bands[0].filter_type, FilterType::HighPass);
    assert!(js_object_to_json(js).contains(r#""note": "a: b, }\" x""#), "strings stay untouched");
  }

  #[test]
  fn sonar_vad_3_is_the_microphone_and_4_is_media() {
    assert_eq!(map_sonar_vad(3), VadType::Mic);
    assert_eq!(map_sonar_vad(4), VadType::Media);
    assert_eq!(map_sonar_vad(2), VadType::Chat);
    assert_eq!(map_sonar_vad(5), VadType::Aux);
    assert_eq!(map_sonar_vad(99), VadType::Game);
  }
}
