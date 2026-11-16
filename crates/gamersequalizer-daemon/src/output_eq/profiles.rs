//! The profiles of the four layers: picking, reading, saving, adding and deleting them.

use super::*;

/// True when two profile names are one profile: the same file on Windows, whose file names
/// ignore case also beyond ASCII ("Řeč" = "řeč"), after the same sanitizing as the file name.
/// Every name-collision check (mic presets, output layers, AutoEQ) goes through this.
pub fn same_profile_slot(a: &str, b: &str) -> bool {
  let key = |n: &str| ProfileStorage::sanitize_filename(n.trim()).to_lowercase();
  key(a) == key(b)
}

impl OutputEq {
  pub fn headphone_profile_name(&self) -> Option<&str> {
    let device = self.output_device.as_ref()?;
    self.settings.headphone_by_device.get(device).map(String::as_str)
  }

  pub fn headphone_profile(&self) -> Option<&ParametricProfile> {
    let name = self.headphone_profile_name()?;
    self.headphone_profiles.iter().find(|p| p.name == name)
  }

  /// Detected game's profile, else the fallback.
  pub fn game_profile_name(&self) -> Option<&str> {
    match self.active_game.as_ref() {
      Some(game) if !game.profile.is_empty() => Some(game.profile.as_str()),
      _ => self.settings.fallback_game_profile.as_deref(),
    }
  }

  pub fn game_profile(&self) -> Option<&ParametricProfile> {
    let name = self.game_profile_name()?;
    self.game_profiles.iter().find(|p| p.name == name)
  }

  pub fn game_profile_mut(&mut self) -> Option<&mut ParametricProfile> {
    let name = self.game_profile_name()?.to_string();
    self.game_profiles.iter_mut().find(|p| p.name == name)
  }

  pub fn headphone_profile_mut(&mut self) -> Option<&mut ParametricProfile> {
    let name = self.headphone_profile_name()?.to_string();
    self.headphone_profiles.iter_mut().find(|p| p.name == name)
  }

  /// Chat / media profiles to pick from; empty for other buses.
  pub fn bus_profiles(&self, bus: StreamBus) -> &[ParametricProfile] {
    match bus {
      StreamBus::Chat => &self.chat_profiles,
      StreamBus::Media => &self.media_profiles,
      _ => &[],
    }
  }

  pub fn bus_enabled(&self, bus: StreamBus) -> bool {
    match bus {
      StreamBus::Chat => self.settings.chat_enabled,
      StreamBus::Media => self.settings.media_enabled,
      _ => false,
    }
  }

  pub fn bus_profile_name(&self, bus: StreamBus) -> Option<&str> {
    match bus {
      StreamBus::Chat => self.settings.chat_profile.as_deref(),
      StreamBus::Media => self.settings.media_profile.as_deref(),
      _ => None,
    }
  }

  pub fn bus_profile(&self, bus: StreamBus) -> Option<&ParametricProfile> {
    let name = self.bus_profile_name(bus)?;
    self.bus_profiles(bus).iter().find(|p| p.name == name)
  }

  pub fn bus_profile_mut(&mut self, bus: StreamBus) -> Option<&mut ParametricProfile> {
    let name = self.bus_profile_name(bus)?.to_string();
    let list = match bus {
      StreamBus::Chat => &mut self.chat_profiles,
      StreamBus::Media => &mut self.media_profiles,
      _ => return None,
    };
    list.iter_mut().find(|p| p.name == name)
  }

  pub fn layer_profiles(&self, layer: OutputLayer) -> &[ParametricProfile] {
    match layer {
      OutputLayer::Headphone => &self.headphone_profiles,
      OutputLayer::Game => &self.game_profiles,
      OutputLayer::Chat => &self.chat_profiles,
      OutputLayer::Media => &self.media_profiles,
    }
  }

  pub fn layer_profile_name(&self, layer: OutputLayer) -> Option<&str> {
    match layer {
      OutputLayer::Headphone => self.headphone_profile_name(),
      OutputLayer::Game => self.game_profile_name(),
      OutputLayer::Chat => self.bus_profile_name(StreamBus::Chat),
      OutputLayer::Media => self.bus_profile_name(StreamBus::Media),
    }
  }

  pub fn layer_profile(&self, layer: OutputLayer) -> Option<&ParametricProfile> {
    match layer {
      OutputLayer::Headphone => self.headphone_profile(),
      OutputLayer::Game => self.game_profile(),
      OutputLayer::Chat => self.bus_profile(StreamBus::Chat),
      OutputLayer::Media => self.bus_profile(StreamBus::Media),
    }
  }

  pub fn layer_profile_mut(&mut self, layer: OutputLayer) -> Option<&mut ParametricProfile> {
    match layer {
      OutputLayer::Headphone => self.headphone_profile_mut(),
      OutputLayer::Game => self.game_profile_mut(),
      OutputLayer::Chat => self.bus_profile_mut(StreamBus::Chat),
      OutputLayer::Media => self.bus_profile_mut(StreamBus::Media),
    }
  }

  /// True when a profile in `layer` already uses this name (or the same file on Windows).
  pub fn layer_profile_exists(&self, layer: OutputLayer, name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && self.layer_profiles(layer).iter().any(|p| same_profile_slot(&p.name, trimmed))
  }

  /// Select the chat / media profile (None = flat).
  pub fn select_bus_profile(&mut self, bus: StreamBus, name: Option<String>, shm: Option<&ShmHeader>) {
    match bus {
      StreamBus::Chat => self.settings.chat_profile = name,
      StreamBus::Media => self.settings.media_profile = name,
      _ => return,
    }
    self.save_settings(shm);
  }

  pub fn save_bus_profile(&mut self, bus: StreamBus, shm: Option<&ShmHeader>) {
    if let Some(p) = self.bus_profile(bus).cloned()
      && let Err(e) = self.storage.save_profile(&p)
    {
      warn!("[OutputEq] could not save {bus:?} profile {}: {e}", p.name);
    }
    self.notify(shm);
  }

  /// Save the active profile's edits in `layer` (after a slider is released).
  pub fn save_layer_profile(&mut self, layer: OutputLayer, shm: Option<&ShmHeader>) {
    match layer {
      OutputLayer::Headphone => self.save_headphone_profile(shm),
      OutputLayer::Game => self.save_game_profile(shm),
      OutputLayer::Chat => self.save_bus_profile(StreamBus::Chat, shm),
      OutputLayer::Media => self.save_bus_profile(StreamBus::Media, shm),
    }
  }

  pub fn delete_layer_profile(
    &mut self,
    layer: OutputLayer,
    name: &str,
    shm: Option<&ShmHeader>,
  ) -> Result<PathBuf, String> {
    match layer {
      OutputLayer::Headphone => self.delete_headphone_profile(name, shm),
      OutputLayer::Game => self.delete_game_profile(name, shm),
      OutputLayer::Chat => self.delete_bus_profile(StreamBus::Chat, name, shm),
      OutputLayer::Media => self.delete_bus_profile(StreamBus::Media, name, shm),
    }
  }

  /// Add or replace a profile in `layer` and select it, except on the Game layer while a
  /// detected game has its own profile: game bindings only change on the user's explicit pick.
  /// A replaced profile keeps its name's spelling (bindings and selections refer to it), its
  /// launcher ids (game detection) and whether it is the user's own.
  pub fn add_layer_profile(
    &mut self,
    layer: OutputLayer,
    mut profile: ParametricProfile,
    shm: Option<&ShmHeader>,
  ) -> Result<(), String> {
    let trimmed = profile.name.trim().to_string();
    if trimmed.is_empty() {
      return Err("profile name cannot be empty".into());
    }
    profile.name = trimmed;
    if let Some(old) = self.layer_profiles(layer).iter().find(|p| same_profile_slot(&p.name, &profile.name)) {
      profile.name = old.name.clone();
      profile.game_ids = old.game_ids.clone();
      profile.is_custom = old.is_custom;
    }
    let name = profile.name.clone();
    for b in &mut profile.bands {
      b.clamp_parameters();
    }
    if let Some(vad) = layer.vad() {
      profile.vad_type = vad;
      self.storage.save_profile(&profile).map_err(|e| e.to_string())?;
    } else {
      self.save_headphone_to_disk(&profile)?;
    }

    let list = match layer {
      OutputLayer::Headphone => &mut self.headphone_profiles,
      OutputLayer::Game => &mut self.game_profiles,
      OutputLayer::Chat => &mut self.chat_profiles,
      OutputLayer::Media => &mut self.media_profiles,
    };
    list.retain(|p| !same_profile_slot(&p.name, &name));
    list.push(profile);
    list.sort_by_key(|p| p.name.to_lowercase());

    match layer {
      OutputLayer::Headphone => self.select_headphone(Some(name), shm),
      OutputLayer::Game => {
        if self.active_game.as_ref().is_none_or(|g| g.profile.is_empty()) {
          self.settings.fallback_game_profile = Some(name);
        }
        self.save_settings(shm);
      }
      OutputLayer::Chat => self.select_bus_profile(StreamBus::Chat, Some(name), shm),
      OutputLayer::Media => self.select_bus_profile(StreamBus::Media, Some(name), shm),
    }
    Ok(())
  }

  /// Save the active headphone profile's edits (after a slider is released).
  pub fn save_headphone_profile(&mut self, shm: Option<&ShmHeader>) {
    if let Some(p) = self.headphone_profile().cloned()
      && let Err(e) = self.save_headphone_to_disk(&p)
    {
      warn!("[OutputEq] {e}");
    }
    self.notify(shm);
  }

  pub fn save_game_profile(&mut self, shm: Option<&ShmHeader>) {
    if let Some(p) = self.game_profile().cloned()
      && let Err(e) = self.storage.save_profile(&p)
    {
      warn!("[OutputEq] could not save game profile {}: {e}", p.name);
    }
    self.notify(shm);
  }

  /// Moves the profile file to `profiles/_deleted` (recoverable) and forgets every reference
  /// to it. Returns where the file went.
  pub fn delete_headphone_profile(&mut self, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let path =
      self.storage.get_headphones_directory().join(format!("{}.json", ProfileStorage::sanitize_filename(name)));
    let moved = self.storage.move_to_deleted(&path)?;
    self.headphone_profiles.retain(|p| p.name != name);
    self.settings.headphone_by_device.retain(|_, p| p != name);
    self.save_settings(shm);
    Ok(moved)
  }

  /// Same for a game profile; also drops bindings and the fallback that point to it.
  pub fn delete_game_profile(&mut self, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let dir = self.storage.get_vad_directory(VadType::Game);
    let moved = self.storage.move_to_deleted(&dir.join(format!("{}.json", ProfileStorage::sanitize_filename(name))))?;
    self.game_profiles.retain(|p| p.name != name);
    self.settings.bindings.retain(|_, p| p != name);
    if self.settings.fallback_game_profile.as_deref() == Some(name) {
      self.settings.fallback_game_profile = None;
    }
    self.save_settings(shm);
    Ok(moved)
  }

  /// Same for a chat / media profile; the channel falls back to flat if it was selected.
  pub fn delete_bus_profile(&mut self, bus: StreamBus, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let vad = match bus {
      StreamBus::Chat => VadType::Chat,
      StreamBus::Media => VadType::Media,
      _ => return Err(format!("{bus:?} has no profiles")),
    };
    let dir = self.storage.get_vad_directory(vad);
    let moved = self.storage.move_to_deleted(&dir.join(format!("{}.json", ProfileStorage::sanitize_filename(name))))?;
    let (list, selected) = match bus {
      StreamBus::Chat => (&mut self.chat_profiles, &mut self.settings.chat_profile),
      _ => (&mut self.media_profiles, &mut self.settings.media_profile),
    };
    list.retain(|p| p.name != name);
    if selected.as_deref() == Some(name) {
      *selected = None;
    }
    self.save_settings(shm);
    Ok(moved)
  }

  /// Add or replace a headphone profile (AutoEQ import) and use it on the current output.
  pub fn add_headphone_profile(&mut self, profile: ParametricProfile, shm: Option<&ShmHeader>) {
    if let Err(e) = self.add_layer_profile(OutputLayer::Headphone, profile, shm) {
      warn!("[OutputEq] could not add headphone profile: {e}");
    }
  }

  pub fn select_headphone(&mut self, name: Option<String>, shm: Option<&ShmHeader>) {
    let Some(device) = self.output_device.clone() else {
      warn!("[OutputEq] no output device: headphone profile not assigned");
      return;
    };
    match name {
      Some(n) => self.settings.headphone_by_device.insert(device, n),
      None => self.settings.headphone_by_device.remove(&device),
    };
    self.save_settings(shm);
  }

  fn save_headphone_to_disk(&self, profile: &ParametricProfile) -> Result<(), String> {
    let dir = self.storage.get_headphones_directory();
    let path = dir.join(format!("{}.json", ProfileStorage::sanitize_filename(&profile.name)));
    serde_json::to_string_pretty(profile).map_err(|e| e.to_string()).and_then(|json| {
      settings::write_atomic(&path, &json).map_err(|e| format!("could not save {}: {e}", path.display()))
    })
  }
}
