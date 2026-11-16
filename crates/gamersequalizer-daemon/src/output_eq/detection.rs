//! Game detection: the foreground app, the active game and the apps seen recently.

use super::*;

/// How often the detection owner checks that the active game is still running.
const ALIVE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Our own windows never count as a game or a recent app.
const OWN_EXE: &str = "gamersequalizer.exe";

/// A foreground app seen recently, for "assign this app to a profile".
#[derive(Debug, Clone, PartialEq)]
pub struct RecentApp {
  pub exe_name: String,
  pub label: String,
  /// How it was classified when it came to the foreground.
  pub matched: Option<GameMatch>,
}

impl OutputEq {
  /// Executable name of the currently detected game when known from recent foreground events.
  pub fn active_game_exe(&self, label: &str) -> Option<String> {
    self.recent_apps.iter().find(|a| a.label == label).map(|a| a.exe_name.clone())
  }

  /// Bind an exe to a profile ("" = not a game). Re-resolves if it is the active game.
  pub fn bind_exe(&mut self, exe_name: &str, profile: Option<String>, shm: Option<&ShmHeader>) {
    let exe = exe_name.to_lowercase();
    match profile {
      Some(p) => self.settings.bindings.insert(exe.clone(), p),
      None => self.settings.bindings.remove(&exe),
    };
    for app in self.recent_apps.iter_mut().filter(|a| a.exe_name == exe) {
      app.matched = None;
    }
    self.save_settings(shm);
  }

  /// A window came to the foreground. `owner`: this process decides the active game.
  pub fn on_foreground(&mut self, event: &GameForegroundEvent, owner: bool, shm: Option<&ShmHeader>) {
    let exe = event.exe_name.to_lowercase();
    if exe == OWN_EXE {
      if owner {
        info!("[Game] foreground: this app, ignored; {}", self.active_game_text());
      }
      return;
    }
    let facts = self.facts_for(&event.full_path, owner);
    let matched = game_match::resolve(&self.settings, &self.game_profiles, &facts);
    let label = matched.as_ref().map(|m| m.label.clone()).unwrap_or_else(|| exe.clone());
    self.recent_apps.retain(|a| a.exe_name != exe);
    if self.recent_apps.len() >= RECENT_APPS {
      self.recent_apps.pop_back();
    }
    self.recent_apps.push_front(RecentApp { exe_name: exe.clone(), label, matched: matched.clone() });

    // Non-games (browser, Discord, this app) leave the active game in place.
    self.is_owner = owner;
    if !owner {
      return;
    }
    // Its sound goes to Game only when it is a game; anything else is Media (`bus_for_exe`). The
    // owner alone says so: a follower knows only what the owner's genre cache holds, and would
    // call a Steam tool that is not in it yet a game.
    crate::win_audio::app_channels::set_detected_game(&exe, matched.is_some());
    let how = game_match::explain(&self.settings, &facts, matched.as_ref());
    match matched {
      Some(m) => {
        info!("[Game] foreground: {exe}: {how}");
        let game = ActiveGame { profile: m.profile.clone().unwrap_or_default(), label: m.label.clone() };
        self.tracked = Some((event.pid, event.full_path.clone()));
        self.set_active_game(Some(game), shm);
      }
      None => info!("[Game] foreground: {exe}: {how}; {}", self.active_game_text()),
    }
  }

  /// Call often (UI frame / daemon loop). Follows the other process and the device.
  pub fn tick(&mut self, owner: bool, output_device: Option<String>, shm: Option<&ShmHeader>) {
    let mut dirty = false;
    self.is_owner = owner;
    if output_device != self.output_device {
      self.output_device = output_device;
      dirty = true;
    }
    if let Some(h) = shm {
      let version = h.out_settings_version.load(Ordering::Acquire);
      if self.seen_settings_version != Some(version) {
        // First tick only records the version; files were just loaded in `new`.
        if self.seen_settings_version.is_some() {
          self.reload();
          self.re_resolve(shm);
        }
        self.seen_settings_version = Some(version);
        dirty = true;
      }
      if !owner {
        let published = h.read_active_game();
        if published != self.active_game {
          self.active_game = published;
          dirty = true;
        }
      } else if self.active_game.is_some() && h.read_active_game().is_none() {
        // Only the owner publishes a game, so a cleared one is the window's "Reset": without
        // taking it over, the window showed no game while the sound kept the game's EQ.
        info!("[Game] active game reset in the window");
        self.tracked = None;
        self.active_game = None;
        dirty = true;
      }
    }
    if owner && self.tracked.is_some() && self.last_alive_check.elapsed() >= ALIVE_CHECK_INTERVAL {
      self.last_alive_check = Instant::now();
      if self.tracked.as_ref().is_some_and(|(pid, _)| !is_process_running(*pid)) {
        info!("[Game] game exited");
        self.tracked = None;
        self.set_active_game(None, shm);
        return;
      }
    }
    if dirty {
      self.apply(shm);
    }
  }

  /// Forget the active game ("Reset"); the owner takes a follower's reset over in `tick`. The
  /// game comes back when it is in the foreground again.
  pub fn clear_active_game(&mut self, shm: Option<&ShmHeader>) {
    self.tracked = None;
    self.set_active_game(None, shm);
  }

  /// Launcher facts and genre of an exe, gathered once per path (cleared by `reload`). Not the
  /// `owner`: its launcher facts are kept too (disk and registry reads), but the genre comes from
  /// the owner's cache each time, so a later event picks up what the owner has looked up since.
  fn facts_for(&mut self, full_path: &str, owner: bool) -> GameFacts {
    let launcher = match self.facts_cache.get(full_path) {
      Some((facts, true)) => return facts.clone(),
      Some((facts, false)) => facts.clone(),
      None => launchers::gather_facts(full_path),
    };
    let mut facts = launcher.clone();
    // Always looked up: it also tells Steam tools from games, whatever the genre switch says.
    let found = if owner { self.genres.lookup(full_path, &facts) } else { self.genres.cached(full_path, &facts) };
    facts.not_a_game = found.not_a_game;
    if self.settings.detect_genres {
      facts.genre = found.genre;
    }
    let kept = if owner { (facts.clone(), true) } else { (launcher, false) };
    self.facts_cache.insert(full_path.to_string(), kept);
    facts
  }

  /// Owner only: classify the tracked game again after bindings / switches changed.
  pub(super) fn re_resolve(&mut self, shm: Option<&ShmHeader>) {
    let Some((_, path)) = self.tracked.clone().filter(|_| self.is_owner) else {
      return;
    };
    let facts = self.facts_for(&path, true);
    match game_match::resolve(&self.settings, &self.game_profiles, &facts) {
      Some(m) => {
        let game = ActiveGame { profile: m.profile.unwrap_or_default(), label: m.label };
        if self.active_game.as_ref() != Some(&game) {
          self.set_active_game(Some(game), shm);
        }
      }
      None => {
        self.tracked = None;
        self.set_active_game(None, shm);
      }
    }
  }

  fn set_active_game(&mut self, game: Option<ActiveGame>, shm: Option<&ShmHeader>) {
    if game != self.active_game {
      info!(
        "[Game] active game changed: {} -> {}",
        self.game_text(self.active_game.as_ref()),
        self.game_text(game.as_ref())
      );
    }
    if let Some(h) = shm {
      h.write_active_game(game.as_ref());
    }
    self.active_game = game;
    self.apply(shm);
  }

  /// For the log: what stays in effect when a non-game comes to the foreground.
  fn active_game_text(&self) -> String {
    format!("active game unchanged: {}", self.game_text(self.active_game.as_ref()))
  }

  /// "Label [profile P]" / "none [fallback profile F]", same choice as `game_profile_name`.
  fn game_text(&self, game: Option<&ActiveGame>) -> String {
    let fallback = self.settings.fallback_game_profile.as_deref();
    match game {
      Some(g) if !g.profile.is_empty() => format!("{} [profile {}]", g.label, g.profile),
      Some(g) => match fallback {
        Some(f) => format!("{} [no own profile, fallback {f}]", g.label),
        None => format!("{} [no profile]", g.label),
      },
      None => match fallback {
        Some(f) => format!("none [fallback {f}]"),
        None => "none [no game EQ]".to_string(),
      },
    }
  }
}
