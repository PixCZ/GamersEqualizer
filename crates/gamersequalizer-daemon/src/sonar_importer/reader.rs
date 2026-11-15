use super::converter::convert_sonar_config;
use super::winsqlite::{WinSqliteConn, WinSqliteLib};
use gamersequalizer_common::profile::ParametricProfile;
use std::collections::HashSet;
use std::path::Path;

pub const DEFAULT_SONAR_DB_PATH: &str = r"C:\ProgramData\SteelSeries\GG\apps\sonar\db\database.db";

pub struct SonarDbReader;

impl SonarDbReader {
  /// Read all profiles from a Sonar SQLite database file using Windows built-in winsqlite3.dll
  pub fn read_all_profiles(db_path: &Path) -> Result<Vec<ParametricProfile>, String> {
    if !db_path.exists() {
      return Err(format!("Sonar database not found at {:?}", db_path));
    }

    let sqlite_lib = WinSqliteLib::load()?;
    let conn = sqlite_lib.open_readonly(db_path)?;
    profiles_from(&conn)
  }

  /// Sonar's database plus the presets of GG's scripts it lacks (Sonar never run, or not since
  /// a GG update). The database wins on equal names: it holds the user's edits. Err only when
  /// neither source gives anything.
  pub fn read_all_sources(db_path: &Path, scripts_dir: &Path) -> Result<Vec<ParametricProfile>, String> {
    let db = if db_path.exists() { Self::read_all_profiles(db_path) } else { Ok(Vec::new()) };
    let scripts = if scripts_dir.exists() { super::migrations::read_presets(scripts_dir) } else { Ok(Vec::new()) };
    match (db, scripts) {
      (Err(a), Err(b)) => Err(format!("{a}; {b}")),
      (db, scripts) => {
        let db = db.unwrap_or_else(|e| {
          log::warn!("[Sonar] database not readable, using GG's scripts only: {e}");
          Vec::new()
        });
        let scripts = scripts.unwrap_or_else(|e| {
          log::warn!("[Sonar] GG's preset scripts not readable, using the database only: {e}");
          Vec::new()
        });
        Ok(merge(db, scripts))
      }
    }
  }
}

/// `db` plus the `scripts` profiles whose file name (per channel) it does not have yet.
fn merge(mut db: Vec<ParametricProfile>, scripts: Vec<ParametricProfile>) -> Vec<ParametricProfile> {
  let mut seen: HashSet<String> = db.iter().map(super::profile_key).collect();
  db.extend(scripts.into_iter().filter(|p| seen.insert(super::profile_key(p))));
  db
}

/// Every Sonar config of `conn` that converts; broken rows are skipped.
pub(super) fn profiles_from(conn: &WinSqliteConn<'_>) -> Result<Vec<ParametricProfile>, String> {
  let mut profiles = Vec::new();
  for (name, vad, is_preset, data, game_ids) in conn.query_configs()? {
    if let Ok(mut profile) = convert_sonar_config(&name, vad, is_preset, &data) {
      profile.game_ids = parse_game_ids(&game_ids);
      profiles.push(profile);
    }
  }
  Ok(profiles)
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

  #[test]
  fn database_wins_and_scripts_add_only_missing_names() {
    use gamersequalizer_common::profile::VadType;
    let mut edited = ParametricProfile::new("Shooter", VadType::Game);
    edited.preamp_db = -3.0;
    let db = vec![edited];
    let scripts = vec![
      ParametricProfile::new("shooter", VadType::Game),
      ParametricProfile::new("Shooter", VadType::Media),
      ParametricProfile::new("New in GG", VadType::Game),
    ];
    let merged = merge(db, scripts);
    let names: Vec<_> = merged.iter().map(|p| (p.name.as_str(), p.vad_type)).collect();
    assert_eq!(names, [("Shooter", VadType::Game), ("Shooter", VadType::Media), ("New in GG", VadType::Game)]);
    assert_eq!(merged[0].preamp_db, -3.0, "the database copy (user's edit) stays");
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
