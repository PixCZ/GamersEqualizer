pub mod converter;
pub mod reader;
pub mod storage;
pub mod winsqlite;

pub use converter::convert_sonar_config;
pub use reader::{SonarDbReader, DEFAULT_SONAR_DB_PATH};
pub use storage::ProfileStorage;

use std::path::Path;

pub struct ImportStats {
  pub total_imported: usize,
  pub game_presets: usize,
  pub custom_presets: usize,
}

pub fn import_sonar_db(db_path: &Path, storage: &ProfileStorage) -> Result<ImportStats, String> {
  let profiles = SonarDbReader::read_all_profiles(db_path)?;
  let mut stats = ImportStats { total_imported: 0, game_presets: 0, custom_presets: 0 };

  for profile in &profiles {
    if let Ok(_) = storage.save_profile(profile) {
      stats.total_imported += 1;
      if profile.vad_type == gamersequalizer_common::profile::VadType::Game {
        stats.game_presets += 1;
      }
      if profile.is_custom {
        stats.custom_presets += 1;
      }
    }
  }

  Ok(stats)
}
