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

  /// The fixture is made up (`test_data/make_sonar_fixture.py`), in the layout of a Sonar database.
  #[test]
  fn reads_a_sonar_database_file() {
    use gamersequalizer_common::profile::{FilterType, VadType};
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/sonar_fixture.db");
    let profiles = SonarDbReader::read_all_profiles(&fixture).unwrap();
    // 8 rows, the one with broken JSON is skipped.
    assert_eq!(profiles.len(), 7);
    let by_name = |name: &str| profiles.iter().find(|p| p.name == name).unwrap();

    let shooter = by_name("Fixture Shooter");
    assert_eq!(shooter.vad_type, VadType::Game);
    assert!(!shooter.is_custom);
    assert_eq!(shooter.game_ids, vec!["1_100001", "15_Fixture Shooter"]);
    assert_eq!(shooter.preamp_db, -1.5);
    assert_eq!(shooter.bands.len(), 2);
    assert_eq!(shooter.bands[0].filter_type, FilterType::LowShelf);
    assert_eq!(shooter.bands[1].frequency, 3200.0);

    assert!(by_name("My game tweak").is_custom);
    assert_eq!(by_name("Fixture Voice").vad_type, VadType::Mic);
    assert_eq!(by_name("Fixture Music").vad_type, VadType::Media);
    assert!(by_name("No EQ block").bands.is_empty());
  }
}
