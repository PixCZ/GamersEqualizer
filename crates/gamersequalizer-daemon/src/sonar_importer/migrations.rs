//! Sonar's presets from the SQL scripts SteelSeries GG ships (`db-migrations\sonar-X.Y.Z\*.sql`).
//!
//! Sonar copies them into its database only when it runs. Someone who keeps GG for their
//! SteelSeries hardware (Engine) but never starts Sonar (it clashes with Dolby Atmos) has no
//! or an old database; replaying the scripts in an in-memory database gives the presets of
//! the installed GG version. GG's files are only read.
//!
//! shortcut: the scripts are SteelSeries' internal format (goose migrations); if a GG update
//! changes it, reading fails with a warning and the database alone is used.

use super::reader::profiles_from;
use super::winsqlite::WinSqliteLib;
use gamersequalizer_common::profile::ParametricProfile;
use std::path::{Path, PathBuf};

/// One script is a few KB today; the cap guards the read, not GG's real sizes.
const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;
const GOOSE_DOWN: &str = "-- +goose Down";

/// `%ProgramFiles%\SteelSeries\GG\apps\sonar\db-migrations`.
pub fn default_dir() -> PathBuf {
  let program_files =
    std::env::var_os("ProgramFiles").map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
  program_files.join(r"SteelSeries\GG\apps\sonar\db-migrations")
}

/// The presets the scripts in `dir` create, run in version order.
pub fn read_presets(dir: &Path) -> Result<Vec<ParametricProfile>, String> {
  let scripts = ordered_scripts(dir)?;
  if scripts.is_empty() {
    return Err(format!("no Sonar scripts in {}", dir.display()));
  }
  let lib = WinSqliteLib::load()?;
  let conn = lib.open_memory()?;
  for path in &scripts {
    let size = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if size > MAX_SCRIPT_BYTES {
      return Err(format!("{} is {size} bytes, too large", path.display()));
    }
    let sql = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.exec(up_section(&sql)).map_err(|e| format!("{}: {e}", path.display()))?;
  }
  profiles_from(&conn)
}

/// `sonar-X.Y.Z` folders by version (0.10.0 after 0.9.0), the `.sql` files in each by name
/// (they start with a timestamp).
fn ordered_scripts(dir: &Path) -> Result<Vec<PathBuf>, String> {
  let mut versions: Vec<((u32, u32, u32), PathBuf)> = std::fs::read_dir(dir)
    .map_err(|e| format!("{}: {e}", dir.display()))?
    .flatten()
    .filter_map(|e| Some((version(&e.file_name().to_string_lossy())?, e.path())))
    .collect();
  versions.sort();
  let mut scripts = Vec::new();
  for (_, folder) in versions {
    let mut in_folder: Vec<PathBuf> = std::fs::read_dir(&folder)
      .map_err(|e| format!("{}: {e}", folder.display()))?
      .flatten()
      .map(|e| e.path())
      .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sql")))
      .collect();
    in_folder.sort();
    scripts.extend(in_folder);
  }
  Ok(scripts)
}

/// "sonar-1.4.0" / "Sonar-1.4.0" -> (1, 4, 0).
fn version(folder: &str) -> Option<(u32, u32, u32)> {
  let rest = folder.get(..6).filter(|p| p.eq_ignore_ascii_case("sonar-")).and(folder.get(6..))?;
  let mut parts = rest.split('.').map(|n| n.parse::<u32>().ok());
  let v = (parts.next()??, parts.next()??, parts.next()??);
  parts.next().is_none().then_some(v)
}

/// Only the "Up" part runs; "Down" would undo the migration.
fn up_section(sql: &str) -> &str {
  sql.find(GOOSE_DOWN).and_then(|i| sql.get(..i)).unwrap_or(sql)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn folders_sort_by_version_and_only_up_runs() {
    assert_eq!(version("sonar-0.10.0"), Some((0, 10, 0)));
    assert_eq!(version("Sonar-1.4.0"), Some((1, 4, 0)));
    assert!(version("sonar-1.4").is_none() && version("sonar-1.4.0.1").is_none() && version("other").is_none());
    assert!(version("sonar-0.9.0") < version("sonar-0.10.0"));
    assert_eq!(up_section("-- +goose Up\nA;\n-- +goose Down\nB;"), "-- +goose Up\nA;\n");
    assert_eq!(up_section("A;"), "A;");
  }

  #[test]
  fn scripts_run_in_version_order_in_memory() {
    let dir = std::env::temp_dir().join(format!("ge_sonar_migrations_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let write = |folder: &str, file: &str, sql: &str| {
      std::fs::create_dir_all(dir.join(folder)).unwrap();
      std::fs::write(
        dir.join(folder).join(file),
        format!("-- +goose Up\n{sql}\n-- +goose Down\nDROP TABLE configs;\n"),
      )
      .unwrap();
    };
    // 0.10.0 must run after 0.9.0 (string order would put it first and fail on the missing table).
    write(
      "sonar-0.9.0",
      "1_schema.sql",
      "CREATE TABLE configs (name TEXT, vad INTEGER, data TEXT, is_preset INTEGER, game_ids TEXT NOT NULL DEFAULT '');",
    );
    write(
      "sonar-0.10.0",
      "2_presets.sql",
      r#"INSERT INTO configs (name, vad, data, is_preset) VALUES ('Script Shooter', 1, '{"globalGain":{"value":0},"parametricEQ":{"enabled":false}}', 1);"#,
    );
    write("sonar-0.10.0", "3_ids.sql", "UPDATE configs SET game_ids = '1_730' WHERE name = 'Script Shooter';");
    std::fs::write(dir.join("sonar-0.10.0").join("notes.txt"), "not SQL").unwrap();

    let presets = read_presets(&dir).unwrap();
    assert_eq!(presets.len(), 1);
    assert_eq!(
      (presets[0].name.as_str(), presets[0].game_ids.as_slice()),
      ("Script Shooter", ["1_730".to_string()].as_slice())
    );
    assert!(read_presets(&dir.join("missing")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// SteelSeries GG's own scripts on this PC: `cargo test real_gg_scripts -- --ignored --nocapture`.
  #[test]
  #[ignore]
  fn real_gg_scripts() {
    let presets = read_presets(&default_dir()).unwrap();
    println!("{} presets, e.g. {:?}", presets.len(), presets.iter().take(5).map(|p| &p.name).collect::<Vec<_>>());
    assert!(presets.len() > 100);
  }
}
