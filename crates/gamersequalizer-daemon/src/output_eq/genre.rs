//! Genre of a game without its own profile, so a genre preset (`game_presets`) can stand in.
//! Sources, all local and optional (a missing launcher is skipped, never an error):
//! 1. a built-in table of big games by exe name (works without any launcher),
//! 2. GOG Galaxy's database: genres/themes/tags of GOG games and of every Steam, Ubisoft,
//!    Epic or EA game the user connected in Galaxy,
//! 3. Steam's `appinfo.vdf`: store tags of every app in the Steam library.
//!
//! Results are cached per exe in `genre_cache.json`; the cache is dropped when a source file
//! changes (Galaxy or Steam updated their data).

use super::game_match::GameFacts;
use super::settings::write_atomic;
use crate::i18n::tk;
use crate::sonar_importer::winsqlite::WinSqliteLib;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CACHE_FILE: &str = "genre_cache.json";
const GALAXY_DB: &str = r"C:\ProgramData\GOG.com\Galaxy\storage\galaxy-2.0.db";
const STEAM_KEY: &str = r"Software\Valve\Steam";
const APPINFO_V28: u32 = 0x0756_4428;
const APPINFO_V29: u32 = 0x0756_4429;
/// Bytes between an app's size field and its key-value data (state, time, token, 2 sha1, change).
const APPINFO_ENTRY_HEADER: usize = 60;
/// Nesting limit for the binary key-value tree (a real appinfo is ~6 deep).
const MAX_KV_DEPTH: usize = 32;
/// Bump when the label rules change, so cached genres are worked out again.
const RULES_VERSION: u32 = 3;
/// Steam app types that are games; "Application", "Tool" and the like are not (Lossless Scaling,
/// Game Compressor), so they never take over the active game.
const STEAM_GAME_TYPES: [&str; 2] = ["game", "demo"];

/// Specific labels (lowercase, matched as a whole label), checked in this order: any of them
/// beats every broad label. "tactical" is left out: Steam uses it for strategy and RPGs too.
const SPECIFIC_LABELS: &[(&str, Genre)] = &[
  ("fps", Genre::Shooter),
  ("shooter", Genre::Shooter),
  ("third-person shooter", Genre::Shooter),
  ("battle royale", Genre::Shooter),
  ("hero shooter", Genre::Shooter),
  ("looter shooter", Genre::Shooter),
  ("competitive", Genre::Competitive),
  ("esports", Genre::Competitive),
  ("moba", Genre::Moba),
  ("rts", Genre::Moba),
  ("real time strategy (rts)", Genre::Moba),
  ("tower defense", Genre::Moba),
  ("open world", Genre::OpenWorld),
  ("story rich", Genre::Story),
  ("visual novel", Genre::Story),
  ("point & click", Genre::Story),
  ("point-and-click", Genre::Story),
];

/// Broad labels: the first one in the game's own label order wins (Steam lists tags by votes,
/// so "Strategy, Survival" is a strategy game).
const BROAD_LABELS: &[(&str, Genre)] = &[
  ("strategy", Genre::Moba),
  ("turn-based strategy", Genre::Moba),
  ("4x", Genre::Moba),
  ("sandbox", Genre::OpenWorld),
  ("survival", Genre::OpenWorld),
  ("racing", Genre::OpenWorld),
  ("simulation", Genre::OpenWorld),
  ("simulator", Genre::OpenWorld),
  ("rpg", Genre::Story),
  ("role-playing", Genre::Story),
  ("role-playing (rpg)", Genre::Story),
  ("adventure", Genre::Story),
];

/// Big games by exe name, for launchers without local genre data (Riot, Battle.net, EA, Epic
/// without Galaxy). shortcut: a hand-kept list; extend it when testers report a missing hit.
const KNOWN_EXES: &[(&str, Genre)] = &[
  ("cs2.exe", Genre::Shooter),
  ("minecraft.windows.exe", Genre::OpenWorld),
  ("valorant-win64-shipping.exe", Genre::Shooter),
  ("fortniteclient-win64-shipping.exe", Genre::Shooter),
  ("overwatch.exe", Genre::Shooter),
  ("r5apex.exe", Genre::Shooter),
  ("r5apex_dx12.exe", Genre::Shooter),
  ("cod.exe", Genre::Shooter),
  ("rainbowsix.exe", Genre::Shooter),
  ("rainbowsix_be.exe", Genre::Shooter),
  ("tslgame.exe", Genre::Shooter),
  ("destiny2.exe", Genre::Shooter),
  ("escapefromtarkov.exe", Genre::Shooter),
  ("helldivers2.exe", Genre::Shooter),
  ("warframe.x64.exe", Genre::Shooter),
  ("marvel-win64-shipping.exe", Genre::Shooter),
  ("bf6.exe", Genre::Shooter),
  ("bf2042.exe", Genre::Shooter),
  ("rocketleague.exe", Genre::Competitive),
  ("league of legends.exe", Genre::Moba),
  ("dota2.exe", Genre::Moba),
  ("sc2_x64.exe", Genre::Moba),
  ("aoe2de_s.exe", Genre::Moba),
  ("genshinimpact.exe", Genre::OpenWorld),
  ("gta5.exe", Genre::OpenWorld),
  ("gta5_enhanced.exe", Genre::OpenWorld),
  ("wow.exe", Genre::OpenWorld),
  ("minecraft.windows.exe", Genre::OpenWorld),
  ("eldenring.exe", Genre::OpenWorld),
  ("cyberpunk2077.exe", Genre::OpenWorld),
  ("witcher3.exe", Genre::OpenWorld),
  ("starrail.exe", Genre::Story),
  ("bg3.exe", Genre::Story),
  ("bg3_dx11.exe", Genre::Story),
];

/// Steam store tag ids (as stored in appinfo.vdf) for the labels above.
const STEAM_TAGS: &[(i64, &str)] = &[
  (1663, "fps"),
  (1774, "shooter"),
  (3814, "third-person shooter"),
  (176981, "battle royale"),
  (1708, "tactical"),
  (3878, "competitive"),
  (5055, "esports"),
  (1718, "moba"),
  (1676, "rts"),
  (1645, "tower defense"),
  (1695, "open world"),
  (3810, "sandbox"),
  (1662, "survival"),
  (1742, "story rich"),
  (3799, "visual novel"),
  (122, "rpg"),
  (1698, "point & click"),
  (9, "strategy"),
  (1741, "turn-based strategy"),
  (1670, "4x"),
  (699, "racing"),
  (599, "simulation"),
  (21, "adventure"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Genre {
  Shooter,
  Competitive,
  Moba,
  OpenWorld,
  Story,
}

/// The persisted cache: source file times + one entry per exe path (lowercase).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct CacheFile {
  rules_version: u32,
  galaxy_modified: Option<SystemTime>,
  steam_modified: Option<SystemTime>,
  games: BTreeMap<String, CachedGenre>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedGenre {
  genre: Option<Genre>,
  source: String,
  /// Steam's type when it is not a game ("Application").
  #[serde(default)]
  steam_type: Option<String>,
}

/// What launcher data says about an exe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lookup {
  pub genre: Option<(Genre, String)>,
  /// Why it is no game although a launcher installed it ("Steam lists it as Application").
  pub not_a_game: Option<String>,
}

/// Looks genres up and remembers them (`OutputEq` keeps one).
pub struct GenreLookup {
  path: PathBuf,
  cache: Option<CacheFile>,
}

impl Genre {
  /// The built-in game profile of this genre (`game_presets`).
  pub const fn preset_name(self) -> &'static str {
    match self {
      Genre::Shooter => tk("Genre: Shooter (footsteps)"),
      Genre::Competitive => tk("Genre: Competitive (max. clarity)"),
      Genre::Moba => tk("Genre: MOBA / strategy"),
      Genre::OpenWorld => tk("Genre: Open world (immersion)"),
      Genre::Story => tk("Genre: Story and dialogue"),
    }
  }
}

/// Genre of `labels` (case-insensitive): a specific label first, else the first broad one.
pub fn from_labels<S: AsRef<str>>(labels: &[S]) -> Option<Genre> {
  let lower: Vec<String> = labels.iter().map(|l| l.as_ref().trim().to_lowercase()).collect();
  let genre_of = |list: &[(&str, Genre)], l: &str| list.iter().find(|(key, _)| *key == l).map(|&(_, g)| g);
  SPECIFIC_LABELS
    .iter()
    .find(|(key, _)| lower.iter().any(|l| l == key))
    .map(|&(_, g)| g)
    .or_else(|| lower.iter().find_map(|l| genre_of(BROAD_LABELS, l)))
}

impl GenreLookup {
  pub fn new(profiles_dir: &Path) -> Self {
    Self { path: profiles_dir.join(CACHE_FILE), cache: None }
  }

  /// Genre of the game `facts` describes (exe at `full_path`) and where it came from.
  pub fn lookup(&mut self, full_path: &str, facts: &GameFacts) -> Lookup {
    if let Some(&(_, g)) = KNOWN_EXES.iter().find(|(exe, _)| *exe == facts.exe_name) {
      return Lookup { genre: Some((g, "built-in list".into())), not_a_game: None };
    }
    if !facts.from_launcher {
      return Lookup::default(); // only launcher games are looked up in launcher data
    }
    let galaxy_modified = modified(Path::new(GALAXY_DB));
    let appinfo = steam_appinfo_path();
    let steam_modified = appinfo.as_deref().and_then(modified);
    let path = self.path.clone();
    let cache = self.cache.get_or_insert_with(|| load_cache(&path));
    if cache.rules_version != RULES_VERSION
      || cache.galaxy_modified != galaxy_modified
      || cache.steam_modified != steam_modified
    {
      if !cache.games.is_empty() {
        debug!("[Genre] Galaxy or Steam data or the genre rules changed: cache cleared");
      }
      *cache = CacheFile { rules_version: RULES_VERSION, galaxy_modified, steam_modified, games: BTreeMap::new() };
    }
    let key = full_path.to_lowercase();
    if let Some(hit) = cache.games.get(&key) {
      let genre = hit.genre.map(|g| (g, hit.source.clone()));
      return Lookup { genre, not_a_game: not_a_game(hit.steam_type.as_deref()) };
    }

    // Steam first: its app type tells tools from games. Read once per exe (cached).
    let steam_id = facts.launcher_ids.iter().find_map(|id| id.strip_prefix("1_")?.parse::<u32>().ok());
    let steam = appinfo.as_deref().zip(steam_id).and_then(|(app, id)| steam_app(app, id));
    let steam_type = steam.as_ref().and_then(|a| a.kind.clone()).filter(|k| !is_game_type(k));
    let (galaxy, galaxy_failed) = if steam_type.is_some() {
      (None, false)
    } else {
      match galaxy_labels(facts) {
        Ok(labels) => (labels, false),
        Err(e) => {
          warn!("[Genre] {e}; trying again next time");
          (None, true)
        }
      }
    };
    let steam_labels = steam.map(|a| a.labels);
    let found = if steam_type.is_some() {
      None
    } else {
      galaxy
        .as_deref()
        .and_then(from_labels)
        .map(|g| (g, "GOG Galaxy".to_string()))
        .or_else(|| steam_labels.as_deref().and_then(from_labels).map(|g| (g, "Steam tags".to_string())))
    };
    // Logged once per game (later lookups come from the cache); the labels explain the choice.
    let shown = |l: Option<&Vec<String>>| match l {
      Some(l) if !l.is_empty() => l.join(", "),
      Some(_) => "none".to_string(),
      None => "not found".to_string(),
    };
    let verdict = match (&steam_type, &found) {
      (Some(kind), _) => format!("not a game, Steam type {kind}"),
      (None, Some((g, s))) => format!("{g:?} from {s}"),
      (None, None) => "no genre found".to_string(),
    };
    info!(
      "[Genre] {}: {verdict}; Galaxy labels: {}; Steam tags: {}",
      facts.exe_name,
      shown(galaxy.as_ref()),
      shown(steam_labels.as_ref()),
    );
    let result = Lookup { genre: found.clone(), not_a_game: not_a_game(steam_type.as_deref()) };
    if galaxy_failed {
      return result; // Galaxy goes first: a result without it is not final
    }
    let entry = CachedGenre {
      genre: found.as_ref().map(|(g, _)| *g),
      source: found.as_ref().map(|(_, s)| s.clone()).unwrap_or_default(),
      steam_type,
    };
    cache.games.insert(key, entry);
    match serde_json::to_string_pretty(&*cache) {
      Ok(text) => {
        if let Err(e) = write_atomic(&self.path, &text) {
          warn!("[Genre] could not save {}: {e}", self.path.display());
        }
      }
      Err(e) => warn!("[Genre] could not serialize the cache: {e}"),
    }
    result
  }
}

fn is_game_type(kind: &str) -> bool {
  STEAM_GAME_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind))
}

fn not_a_game(steam_type: Option<&str>) -> Option<String> {
  steam_type.map(|kind| format!("Steam lists it as {kind}"))
}

// --- GOG Galaxy -----------------------------------------------------------------------------

/// Genres, themes and store tags of the game in Galaxy's database: by release key for Steam,
/// GOG and Epic ids, by title for the rest (Galaxy's Ubisoft keys are not the registry ids).
/// Err when the database could not be read (e.g. locked), so the caller does not cache the miss.
fn galaxy_labels(facts: &GameFacts) -> Result<Option<Vec<String>>, String> {
  let db = Path::new(GALAXY_DB);
  if !db.exists() {
    return Ok(None);
  }
  let lib = WinSqliteLib::load()?;
  let conn = lib.open_readonly(db).map_err(|e| format!("Galaxy database: {e}"))?;
  let pieces = |where_clause: &str| {
    conn.query_text(
      &format!(
        "SELECT gp.releaseKey, gp.value FROM GamePieces gp JOIN GamePieceTypes t ON t.id = gp.gamePieceTypeId \
         WHERE t.type IN ('meta', 'storeTags') AND {where_clause}"
      ),
      2,
    )
  };

  let mut keys: Vec<String> = facts.launcher_ids.iter().filter_map(|id| galaxy_release_key(id)).collect();
  if keys.is_empty() {
    let wanted: Vec<String> = facts.names.iter().map(|n| compact(n)).filter(|n| n.len() >= 3).collect();
    let titles = conn
      .query_text(
        "SELECT gp.releaseKey, gp.value FROM GamePieces gp JOIN GamePieceTypes t ON t.id = gp.gamePieceTypeId \
         WHERE t.type = 'title'",
        2,
      )
      .map_err(|e| format!("Galaxy titles: {e}"))?;
    keys = titles
      .into_iter()
      .filter(|row| {
        let title = row.get(1).and_then(|v| json_str(v, "title")).map(|t| compact(&t)).unwrap_or_default();
        !title.is_empty() && wanted.contains(&title)
      })
      .filter_map(|row| row.into_iter().next())
      .filter(|k| is_safe_key(k))
      .collect();
  }
  if keys.is_empty() {
    return Ok(None);
  }
  let list = keys.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
  let rows = pieces(&format!("gp.releaseKey IN ({list})")).map_err(|e| format!("Galaxy pieces: {e}"))?;
  let labels: Vec<String> = rows.iter().filter_map(|row| row.get(1)).flat_map(|v| galaxy_piece_labels(v)).collect();
  Ok((!labels.is_empty()).then_some(labels))
}

/// Our launcher id -> Galaxy release key ("1_730" -> "steam_730"). None for ids Galaxy keys
/// differently, and for anything that is not safe inside an SQL string literal.
fn galaxy_release_key(id: &str) -> Option<String> {
  let (prefix, rest) = id.split_once('_')?;
  let platform = match prefix {
    "1" => "steam",
    "3" => "gog",
    "9" => "epic",
    _ => return None,
  };
  let key = format!("{platform}_{rest}");
  is_safe_key(&key).then_some(key)
}

fn is_safe_key(key: &str) -> bool {
  !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Labels of one `meta` (`genres`, `themes`) or `storeTags` (`tags[].name`) piece.
fn galaxy_piece_labels(value: &str) -> Vec<String> {
  let Ok(v) = serde_json::from_str::<serde_json::Value>(value) else {
    return Vec::new();
  };
  let strings = |key: &str| -> Vec<String> {
    v.get(key).and_then(|a| a.as_array()).into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
  };
  let mut out = strings("genres");
  out.extend(strings("themes"));
  out.extend(
    v.get("tags")
      .and_then(|a| a.as_array())
      .into_iter()
      .flatten()
      .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from)),
  );
  out
}

fn json_str(text: &str, key: &str) -> Option<String> {
  serde_json::from_str::<serde_json::Value>(text).ok()?.get(key)?.as_str().map(String::from)
}

/// Letters and digits only, lowercase: "Far Cry® 6" and "Far Cry 6" compare equal.
fn compact(name: &str) -> String {
  name.chars().filter(|c| c.is_alphanumeric() && c.is_ascii()).collect::<String>().to_lowercase()
}

// --- Steam appinfo.vdf ---------------------------------------------------------------------

fn steam_appinfo_path() -> Option<PathBuf> {
  let dir = super::launchers::hkcu_string(STEAM_KEY, "SteamPath")?;
  let path = PathBuf::from(dir).join("appcache").join("appinfo.vdf");
  path.exists().then_some(path)
}

/// A Steam app as appinfo.vdf describes it.
#[derive(Debug, Default, PartialEq)]
struct SteamApp {
  /// `common/type`: "Game", "Application", "Tool", ...
  kind: Option<String>,
  /// Store tag labels we know, most voted first.
  labels: Vec<String>,
}

fn steam_app(appinfo: &Path, app_id: u32) -> Option<SteamApp> {
  let data = std::fs::read(appinfo).map_err(|e| warn!("[Genre] {}: {e}", appinfo.display())).ok()?;
  match parse_appinfo(&data, app_id) {
    Ok(app) => app.map(|(kind, tags)| SteamApp {
      kind,
      labels: tags
        .iter()
        .filter_map(|id| STEAM_TAGS.iter().find(|(t, _)| t == id))
        .map(|(_, l)| l.to_string())
        .collect(),
    }),
    Err(e) => {
      warn!("[Genre] {} not readable: {e}", appinfo.display());
      None
    }
  }
}

/// Type and store tag ids of one app.
type AppInfoEntry = (Option<String>, Vec<i64>);

/// Type and tag ids of `app_id` in appinfo.vdf (`appinfo/common/type`, `appinfo/common/store_tags`
/// in key order "0", "1", ...). None if the app is not in the file. Versions 28 and 29 only.
fn parse_appinfo(data: &[u8], app_id: u32) -> Result<Option<AppInfoEntry>, String> {
  let mut r = Reader { data, pos: 0 };
  let magic = r.u32()?;
  let _universe = r.u32()?;
  let strings = match magic {
    APPINFO_V29 => {
      let offset = usize::try_from(r.u64()?).map_err(|_| "string table offset")?;
      let mut t = Reader { data, pos: offset };
      let count = t.u32()?;
      (0..count).map(|_| t.cstr()).collect::<Result<Vec<_>, _>>()?
    }
    APPINFO_V28 => Vec::new(),
    other => return Err(format!("unknown appinfo version {other:#x}")),
  };
  loop {
    let id = r.u32()?;
    if id == 0 {
      return Ok(None);
    }
    let size = r.u32()? as usize;
    let start = r.pos;
    if id != app_id {
      r.pos = start.checked_add(size).ok_or("entry size")?;
      continue;
    }
    r.pos = start.checked_add(APPINFO_ENTRY_HEADER).ok_or("entry size")?;
    let tree = r.kv_map(&strings, magic == APPINFO_V29, 0)?;
    let tags = kv_get(&tree, &["appinfo", "common", "store_tags"]);
    let mut out: Vec<(usize, i64)> = match tags {
      Some(Kv::Map(items)) => items
        .iter()
        .filter_map(|(k, v)| match v {
          Kv::Int(n) => Some((k.parse().unwrap_or(usize::MAX), *n)),
          _ => None,
        })
        .collect(),
      _ => Vec::new(),
    };
    out.sort_by_key(|&(rank, _)| rank);
    let kind = match kv_get(&tree, &["appinfo", "common", "type"]) {
      Some(Kv::Str(k)) => Some(k.clone()),
      _ => None,
    };
    return Ok(Some((kind, out.into_iter().map(|(_, id)| id).collect())));
  }
}

enum Kv {
  Map(Vec<(String, Kv)>),
  Int(i64),
  Str(String),
  Other,
}

fn kv_get<'a>(items: &'a [(String, Kv)], path: &[&str]) -> Option<&'a Kv> {
  let (first, rest) = path.split_first()?;
  let found = &items.iter().find(|(k, _)| k.eq_ignore_ascii_case(first))?.1;
  match (rest.is_empty(), found) {
    (true, v) => Some(v),
    (false, Kv::Map(inner)) => kv_get(inner, rest),
    _ => None,
  }
}

struct Reader<'a> {
  data: &'a [u8],
  pos: usize,
}

impl Reader<'_> {
  fn take(&mut self, n: usize) -> Result<&[u8], String> {
    let end = self.pos.checked_add(n).ok_or("overflow")?;
    let bytes = self.data.get(self.pos..end).ok_or("unexpected end of file")?;
    self.pos = end;
    Ok(bytes)
  }

  fn u32(&mut self) -> Result<u32, String> {
    let b = self.take(4)?;
    Ok(u32::from_le_bytes(b.try_into().map_err(|_| "u32")?))
  }

  fn u64(&mut self) -> Result<u64, String> {
    let b = self.take(8)?;
    Ok(u64::from_le_bytes(b.try_into().map_err(|_| "u64")?))
  }

  fn cstr(&mut self) -> Result<String, String> {
    let rest = self.data.get(self.pos..).ok_or("unexpected end of file")?;
    let len = rest.iter().position(|&b| b == 0).ok_or("unterminated string")?;
    let s = String::from_utf8_lossy(rest.get(..len).unwrap_or_default()).into_owned();
    self.pos += len + 1;
    Ok(s)
  }

  /// One binary key-value map, up to its end marker (0x08).
  fn kv_map(&mut self, strings: &[String], indexed_keys: bool, depth: usize) -> Result<Vec<(String, Kv)>, String> {
    if depth > MAX_KV_DEPTH {
      return Err("key-value tree too deep".into());
    }
    let mut items = Vec::new();
    loop {
      let kind = *self.take(1)?.first().ok_or("type")?;
      if kind == 0x08 {
        return Ok(items);
      }
      let key = if indexed_keys {
        let i = self.u32()? as usize;
        strings.get(i).cloned().ok_or("string index out of range")?
      } else {
        self.cstr()?
      };
      let value = match kind {
        0x00 => Kv::Map(self.kv_map(strings, indexed_keys, depth + 1)?),
        0x01 => Kv::Str(self.cstr()?),
        0x02 | 0x04 | 0x06 => Kv::Int(i64::from(self.u32()? as i32)),
        0x03 => {
          self.take(4)?;
          Kv::Other
        }
        0x07 | 0x0A => {
          self.take(8)?;
          Kv::Other
        }
        other => return Err(format!("unknown key-value type {other:#x}")),
      };
      items.push((key, value));
    }
  }
}

// --- cache ---------------------------------------------------------------------------------

fn load_cache(path: &Path) -> CacheFile {
  let Ok(text) = std::fs::read_to_string(path) else {
    return CacheFile::default();
  };
  serde_json::from_str(&text).unwrap_or_else(|e| {
    warn!("[Genre] {} is corrupt ({e}), starting a new cache", path.display());
    CacheFile::default()
  })
}

fn modified(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  #[test]
  fn specific_labels_beat_broad_ones() {
    assert_eq!(from_labels(&["Adventure", "Strategy", "Simulator", "Open world"]), Some(Genre::OpenWorld));
    assert_eq!(from_labels(&["Action", "Adventure", "Shooter"]), Some(Genre::Shooter));
    assert_eq!(from_labels(&["Strategy", "Real Time Strategy (RTS)"]), Some(Genre::Moba));
    assert_eq!(from_labels(&["Role-playing (RPG)", "Indie"]), Some(Genre::Story));
    assert_eq!(from_labels(&["Indie", "Puzzle"]), None);
    // Star Trek: Voyager - Across the Unknown: broad tags in vote order, "tactical" ignored.
    assert_eq!(from_labels(&["strategy", "survival", "tactical"]), Some(Genre::Moba));
    assert_eq!(from_labels(&["Survival", "Strategy"]), Some(Genre::OpenWorld));
    assert_eq!(from_labels(&["tactical", "fps", "shooter"]), Some(Genre::Shooter));
  }

  #[test]
  fn every_genre_names_a_builtin_preset() {
    let names: Vec<String> = super::super::game_presets::builtin_game_presets().into_iter().map(|p| p.name).collect();
    for g in [Genre::Shooter, Genre::Competitive, Genre::Moba, Genre::OpenWorld, Genre::Story] {
      assert!(names.iter().any(|n| n == g.preset_name()), "{g:?}");
    }
  }

  #[test]
  fn launcher_ids_become_galaxy_keys_safely() {
    assert_eq!(galaxy_release_key("1_730").as_deref(), Some("steam_730"));
    assert_eq!(galaxy_release_key("3_1207658924").as_deref(), Some("gog_1207658924"));
    assert_eq!(galaxy_release_key("9_Fortnite").as_deref(), Some("epic_Fortnite"));
    assert_eq!(galaxy_release_key("8_4932"), None, "Galaxy keys Ubisoft by uuid");
    assert_eq!(galaxy_release_key("1_7'; DROP TABLE x;--"), None);
  }

  #[test]
  fn galaxy_pieces_give_genres_themes_and_tags() {
    let meta = r#"{"genres":["Adventure","Strategy"],"themes":["Open world"],"criticsScore":95}"#;
    assert_eq!(galaxy_piece_labels(meta), ["Adventure", "Strategy", "Open world"]);
    let tags = r#"{"tags":[{"id":"shooter","name":"Shooter"}]}"#;
    assert_eq!(galaxy_piece_labels(tags), ["Shooter"]);
    assert!(galaxy_piece_labels("not json").is_empty());
    assert_eq!(compact("Far Cry\u{fffd} 6"), compact("Far Cry® 6"));
  }

  #[test]
  fn appinfo_v29_tags_in_vote_order() {
    // String table: 0 appinfo, 1 common, 2 store_tags, 3 "0", 4 "1", 5 name, 6 type.
    let strings = ["appinfo", "common", "store_tags", "0", "1", "name", "type"];
    let mut kv = Vec::new();
    let key = |kv: &mut Vec<u8>, kind: u8, i: u32| {
      kv.push(kind);
      kv.extend(i.to_le_bytes());
    };
    key(&mut kv, 0x00, 0); // appinfo {
    key(&mut kv, 0x01, 5); //   name "X"
    kv.extend(b"X\0");
    key(&mut kv, 0x00, 1); //   common {
    key(&mut kv, 0x01, 6); //     type "Game"
    kv.extend(b"Game\0");
    key(&mut kv, 0x00, 2); //     store_tags {
    key(&mut kv, 0x02, 4); //       "1" = 1774
    kv.extend(1774u32.to_le_bytes());
    key(&mut kv, 0x02, 3); //       "0" = 1663
    kv.extend(1663u32.to_le_bytes());
    kv.extend([0x08, 0x08, 0x08, 0x08]); // close store_tags, common, appinfo, root

    let mut file = Vec::new();
    file.extend(APPINFO_V29.to_le_bytes());
    file.extend(1u32.to_le_bytes());
    let offset_at = file.len();
    file.extend(0u64.to_le_bytes());
    // Another app first, skipped by its size.
    file.extend(10u32.to_le_bytes());
    file.extend(3u32.to_le_bytes());
    file.extend([1, 2, 3]);
    file.extend(730u32.to_le_bytes());
    file.extend(((APPINFO_ENTRY_HEADER + kv.len()) as u32).to_le_bytes());
    file.extend(vec![0u8; APPINFO_ENTRY_HEADER]);
    file.extend(&kv);
    file.extend(0u32.to_le_bytes());
    let table = file.len() as u64;
    file.splice(offset_at..offset_at + 8, table.to_le_bytes());
    file.extend((strings.len() as u32).to_le_bytes());
    for s in strings {
      file.extend(s.as_bytes());
      file.push(0);
    }

    assert_eq!(parse_appinfo(&file, 730).unwrap(), Some((Some("Game".to_string()), vec![1663, 1774])));
    assert_eq!(parse_appinfo(&file, 999).unwrap(), None);
    assert!(parse_appinfo(&file[..40], 730).is_err(), "truncated file is an error, not a panic");
    assert!(parse_appinfo(&[0x27, 0x44, 0x56, 0x07, 0, 0, 0, 0], 730).is_err(), "old version");
    assert!(is_game_type("Game") && is_game_type("demo"));
    assert!(!is_game_type("Application") && !is_game_type("Tool"));
  }

  /// Reads this PC's Steam data; run by hand: `cargo test real_steam_appinfo -- --ignored --nocapture`.
  #[test]
  #[ignore]
  fn real_steam_appinfo() {
    let path = steam_appinfo_path().expect("Steam installed");
    // CS2, Dota 2, Satisfactory, Apex, Lossless Scaling, Game Compressor
    for app in [730u32, 570, 526870, 1172470, 993090, 4339880] {
      println!("{app}: {:?}", steam_app(&path, app));
    }
  }

  /// Reads this PC's GOG Galaxy data; run by hand like `real_steam_appinfo`.
  #[test]
  #[ignore]
  fn real_galaxy() {
    for (ids, names) in [(vec!["1_526870"], vec![]), (vec!["8_4932"], vec!["Anno 2205"])] {
      let facts = GameFacts {
        launcher_ids: ids.into_iter().map(String::from).collect(),
        names: names.into_iter().map(String::from).collect(),
        from_launcher: true,
        ..Default::default()
      };
      let labels = galaxy_labels(&facts).unwrap();
      println!("{:?} -> {labels:?} -> {:?}", facts.launcher_ids, labels.as_deref().and_then(from_labels));
    }
  }
}
