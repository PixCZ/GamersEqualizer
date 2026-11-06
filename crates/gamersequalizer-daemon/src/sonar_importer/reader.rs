use super::converter::convert_sonar_config;
use super::winsqlite::WinSqliteLib;
use gamersequalizer_common::profile::ParametricProfile;
use std::path::Path;

pub const DEFAULT_SONAR_DB_PATH: &str = r"C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db";

pub struct SonarDbReader;

impl SonarDbReader {
  /// Check if the default SteelSeries Sonar database exists
  pub fn default_db_exists() -> bool {
    Path::new(DEFAULT_SONAR_DB_PATH).exists()
  }

  /// Read all profiles from a Sonar SQLite database file using Windows built-in winsqlite3.dll
  pub fn read_all_profiles(db_path: &Path) -> Result<Vec<ParametricProfile>, String> {
    if !db_path.exists() {
      return Err(format!("Sonar database not found at {:?}", db_path));
    }

    let sqlite_lib = WinSqliteLib::load()?;
    let conn = sqlite_lib.open_readonly(db_path)?;
    let rows = conn.query_configs()?;

    let mut profiles = Vec::new();
    for (name, vad, is_preset, data, game_ids) in rows {
      if let Ok(mut profile) = convert_sonar_config(&name, vad, is_preset, &data) {
        profile.game_ids = parse_game_ids(&game_ids);
        profiles.push(profile);
      }
    }

    Ok(profiles)
  }
}

/// "1_3932890, 15_Escape from Tarkov" -> ["1_3932890", "15_Escape from Tarkov"]
fn parse_game_ids(raw: &str) -> Vec<String> {
  raw.split(',').map(str::trim).filter(|s| s.contains('_')).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn game_ids_are_split_and_trimmed() {
    assert_eq!(parse_game_ids("1_3932890, 15_Escape from Tarkov,"), vec!["1_3932890", "15_Escape from Tarkov"]);
    assert!(parse_game_ids("").is_empty());
  }
  use std::path::PathBuf;

  #[test]
  fn test_read_real_sonar_database() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .unwrap()
      .parent()
      .unwrap()
      .join("test_data")
      .join("sonar_database.db");

    if fixture.exists() {
      let profiles = SonarDbReader::read_all_profiles(&fixture).unwrap();
      assert!(!profiles.is_empty(), "Profiles should not be empty");
      let game_profiles: Vec<_> =
        profiles.iter().filter(|p| p.vad_type == gamersequalizer_common::profile::VadType::Game).collect();
      assert!(game_profiles.len() >= 50, "Should have dozens of game profiles");
      let division = game_profiles.iter().find(|p| p.name == "The Division 2");
      assert!(division.is_some_and(|p| p.game_ids.iter().any(|id| id == "1_2221490")));
    }
  }
}
