pub mod apo_banner;
pub mod app;
pub mod audio_settings;
pub mod eq_editor;
pub mod eq_graph;
pub mod header;
pub mod mic_presets;
pub mod prefs;
pub mod profile_bar;
pub mod setup;
pub mod state;
pub mod tab_eq;
pub mod tab_headphones;
pub mod tab_log;
pub mod tab_mic;
pub mod tab_mixer;
pub mod tab_settings;
pub mod tab_sidetone;
pub mod theme;
pub mod window_state;

pub use app::GamersEqualizerApp;
pub use state::AppState;

/// Shorten for display by characters, not bytes: byte slicing (`&s[..12]`) panics when the
/// cut lands inside a multi-byte character, e.g. Czech device names ("Sluchátka ...").
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
  if s.chars().count() <= max_chars {
    return s.to_string();
  }
  let kept: String = s.chars().take(max_chars.saturating_sub(2)).collect();
  format!("{kept}...")
}

#[cfg(test)]
mod tests {
  use super::truncate_chars;

  #[test]
  fn test_truncate_chars_multibyte_safe() {
    // Regression: "Sluchátka" with the cut inside "á" used to panic.
    for n in 1..30 {
      let _ = truncate_chars("Sluchátka s mikrofonem (Jabra Evolve2 65)", n);
    }
    assert_eq!(truncate_chars("Krátké", 14), "Krátké");
    assert_eq!(truncate_chars("Náhlavní souprava Jabra", 10), "Náhlavní...");
  }
}
