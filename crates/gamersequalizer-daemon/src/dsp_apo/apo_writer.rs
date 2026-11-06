use gamersequalizer_common::profile::{expand_slopes, BiquadBand};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_APO_CONFIG_PATH: &str = r"C:\Program Files\EqualizerAPO\config\gamersequalizer_live.txt";

pub struct ApoWriter {
  config_path: PathBuf,
}

impl ApoWriter {
  pub fn new(config_path: impl Into<PathBuf>) -> Self {
    Self { config_path: config_path.into() }
  }

  pub fn default_path() -> Self {
    Self::new(DEFAULT_APO_CONFIG_PATH)
  }

  /// Format biquad filter bands and preamp into Equalizer APO syntax
  pub fn format_apo_config(
    preamp_db: f32,
    layer_a_name: &str,
    layer_a_bands: &[BiquadBand],
    layer_b_name: &str,
    layer_b_bands: &[BiquadBand],
  ) -> String {
    let mut out = String::new();
    out.push_str("# GamersEqualizer Live Configuration (Pure Rust)\n");
    out.push_str("# Generated automatically - changes here will be overwritten\n\n");

    // Preamp
    out.push_str(&format!("Preamp: {:.2} dB\n\n", preamp_db));

    // Layer A
    out.push_str(&format!("# Layer A: {}\n", layer_a_name));
    for band in &expand_slopes(layer_a_bands) {
      if band.enabled {
        let abbr = band.filter_type.apo_abbreviation();
        out.push_str(&format!(
          "Filter: ON {} Fc {:.1} Hz Gain {:.2} dB Q {:.3}\n",
          abbr, band.frequency, band.gain_db, band.q
        ));
      }
    }
    out.push('\n');

    // Layer B
    out.push_str(&format!("# Layer B: {}\n", layer_b_name));
    for band in &expand_slopes(layer_b_bands) {
      if band.enabled {
        let abbr = band.filter_type.apo_abbreviation();
        out.push_str(&format!(
          "Filter: ON {} Fc {:.1} Hz Gain {:.2} dB Q {:.3}\n",
          abbr, band.frequency, band.gain_db, band.q
        ));
      }
    }

    out
  }

  /// Write configuration atomically via temporary file rename
  pub fn write_config_atomic(
    &self,
    preamp_db: f32,
    layer_a_name: &str,
    layer_a_bands: &[BiquadBand],
    layer_b_name: &str,
    layer_b_bands: &[BiquadBand],
  ) -> io::Result<()> {
    let content = Self::format_apo_config(preamp_db, layer_a_name, layer_a_bands, layer_b_name, layer_b_bands);

    let parent = self.config_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp_path = self.config_path.with_extension("tmp");
    {
      let mut file = File::create(&tmp_path)?;
      file.write_all(content.as_bytes())?;
      file.sync_all()?;
    }

    // Atomic rename
    fs::rename(&tmp_path, &self.config_path)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::FilterType;

  #[test]
  fn test_apo_syntax_generation() {
    let band_a = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 1000.0,
      gain_db: 3.5,
      q: 1.414,
      slope_db: 12,
    };
    let band_b = BiquadBand {
      enabled: true,
      filter_type: FilterType::HighShelf,
      frequency: 8000.0,
      gain_db: -2.0,
      q: 0.707,
      slope_db: 12,
    };

    let text = ApoWriter::format_apo_config(-4.0, "Rode NTH-100", &[band_a], "CS2 Footsteps", &[band_b]);
    assert!(text.contains("Preamp: -4.00 dB"));
    assert!(text.contains("Filter: ON PK Fc 1000.0 Hz Gain 3.50 dB Q 1.414"));
    assert!(text.contains("Filter: ON HSC Fc 8000.0 Hz Gain -2.00 dB Q 0.707"));
  }

  #[test]
  fn test_atomic_file_write() {
    let dir = std::env::temp_dir().join("opensonar_test_apo");
    let target = dir.join("opensonar_live.txt");
    let writer = ApoWriter::new(&target);

    let band = BiquadBand::default();
    let res = writer.write_config_atomic(-1.0, "Headphones", &[band], "Default", &[]);
    assert!(res.is_ok());

    let content = fs::read_to_string(&target).unwrap();
    assert!(content.contains("Preamp: -1.00 dB"));
    let _ = fs::remove_dir_all(&dir);
  }
}
