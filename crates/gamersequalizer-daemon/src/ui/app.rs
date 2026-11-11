use super::header::{render_nav, render_top_bar, NAV_WIDTH};
use super::state::{ActiveTab, AppState};
use super::tab_eq::render_tab_eq;
use super::tab_mic::render_tab_mic;
use super::tab_mixer::render_tab_mixer;
use crate::win_audio::WinAudioSystem;
use eframe::egui;
use gamersequalizer_common::streams::StreamBus;
use std::time::{Duration, Instant};

/// How often the window checks that the background service still runs (one mutex open).
const DAEMON_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Windows volumes / mic peak are read this often, not every frame (each read is an RPC into
/// the audio service). 10 Hz keeps the meters moving.
const SYSTEM_SYNC_INTERVAL: Duration = Duration::from_millis(100);
/// Frame interval while visible (meters), and while minimized (nothing to show; the game
/// detection and the watchdog still run).
const FRAME_INTERVAL: Duration = Duration::from_millis(30);
const MINIMIZED_FRAME_INTERVAL: Duration = Duration::from_secs(1);

pub struct GamersEqualizerApp {
  pub state: AppState,
  pub win_audio: WinAudioSystem,
  applied_mica: bool,
  window: super::window_state::WindowTracker,
  daemon_checked_at: Instant,
  synced_at: Option<Instant>,
}

impl GamersEqualizerApp {
  pub fn new(state: AppState, win_audio: WinAudioSystem, window: super::window_state::WindowTracker) -> Self {
    Self { state, win_audio, applied_mica: false, window, daemon_checked_at: Instant::now(), synced_at: None }
  }
}

impl eframe::App for GamersEqualizerApp {
  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let ctx = &ui.ctx().clone();
    if !self.applied_mica {
      self.applied_mica = true;
    }

    // 0. The background service carries the echo reference and the mic telemetry. Restart it
    // if it died (or was still exiting when this window started, so the startup check missed it).
    if self.daemon_checked_at.elapsed() >= DAEMON_CHECK_INTERVAL {
      self.daemon_checked_at = Instant::now();
      // The installer just copied this app next to its effects: close, the installed one
      // starts once this window is gone.
      if crate::installed_copy_ready(&self.state) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
      } else {
        crate::ensure_daemon_running();
      }
    }

    // 1. Synchronize system master and mic volume from Windows in real time. A mic level the
    // service's automatic input gain set is not the user's volume.
    if let Some(h) = self.state.shm_header() {
      let auto = h.mic_auto_gain_permille.load(std::sync::atomic::Ordering::Relaxed);
      self.win_audio.chatmix.mic_hw_auto = (auto > 0).then(|| auto as f32 / 1000.0);
    }
    let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
    let sync_due = !minimized && self.synced_at.is_none_or(|t| t.elapsed() >= SYSTEM_SYNC_INTERVAL);
    if sync_due {
      self.synced_at = Some(Instant::now());
      if self.win_audio.chatmix.sync_from_system() {
        self.state.sync_mixer_to_shm(&self.win_audio.chatmix);
      }
    }

    // 2. Game detection: the daemon decides when it runs; this window follows it.
    let owner = !self.state.is_daemon_active();
    if let Some(ref detector) = self.win_audio.game_detector {
      while let Some(event) = detector.poll_event() {
        self.win_audio.chatmix.set_current_game(Some(event.exe_name.clone()));
        self.state.output_eq_do(|eq, shm| eq.on_foreground(&event, owner, shm));
      }
    }
    let output = self.win_audio.chatmix.master_device_id.clone();
    self.state.output_eq_do(|eq, shm| eq.tick(owner, output, shm));
    self.state.active_game_name = self.state.output_eq.active_game.as_ref().map(|g| g.label.clone());
    // Save an output EQ band edit once the slider is released, not on every drag frame.
    if self.state.out_eq_unsaved.is_some() && !ctx.input(|i| i.pointer.any_down()) {
      match self.state.out_eq_unsaved.take() {
        Some(super::state::OutputLayer::Headphone) => self.state.output_eq_do(|eq, shm| eq.save_headphone_profile(shm)),
        Some(super::state::OutputLayer::Game) => self.state.output_eq_do(|eq, shm| eq.save_game_profile(shm)),
        Some(super::state::OutputLayer::Chat) => {
          self.state.output_eq_do(|eq, shm| eq.save_bus_profile(StreamBus::Chat, shm))
        }
        Some(super::state::OutputLayer::Media) => {
          self.state.output_eq_do(|eq, shm| eq.save_bus_profile(StreamBus::Media, shm))
        }
        None => {}
      }
    }

    // 3. Refresh endpoints and track Windows system default device changes in real time
    self.win_audio.endpoint_lock.enforce_lock();
    let dev_changed = self.win_audio.endpoint_lock.poll_device_changed();
    // The defaults are two more RPCs: read them with the volume sync, or when a device changed.
    let poll = sync_due || dev_changed || self.state.endpoints.is_empty();
    let current_sys_default = if poll { self.win_audio.endpoint_lock.get_default_output_id() } else { None };
    let current_sys_comms = if poll { self.win_audio.endpoint_lock.get_default_communications_id() } else { None };
    let is_locked = self.win_audio.endpoint_lock.is_locked();

    if self.state.endpoints.is_empty()
      || dev_changed
      || (!is_locked
        && current_sys_default.is_some()
        && current_sys_default != self.state.selected_endpoint_id
        && self.win_audio.chatmix.master_device_id.as_ref() != self.state.selected_endpoint_id.as_ref())
    {
      self.state.endpoints = self.win_audio.endpoint_lock.enumerate_playback_devices();
      self.state.capture_endpoints = self.win_audio.endpoint_lock.enumerate_capture_devices();
      self.win_audio.chatmix.bind_endpoints(&self.state.endpoints, current_sys_default.as_deref());
      if !is_locked {
        if let Some(ref master_id) = self.win_audio.chatmix.master_device_id {
          self.state.selected_endpoint_id = Some(master_id.clone());
        } else if let Some(ref def_id) = current_sys_default {
          self.state.selected_endpoint_id = Some(def_id.clone());
        }
      }
      if self.state.selected_chat_endpoint_id.is_none() {
        self.state.selected_chat_endpoint_id = self.win_audio.chatmix.chat_device_id.clone().or(current_sys_comms);
      }
      // The mic the user picked while it is connected, else the Windows default that
      // `bind_endpoints` resolved. Only a pick is saved: an unplugged pick comes back on replug,
      // and a guess never sticks (it once kept a laptop mic after a USB headset became default).
      let picked = self
        .state
        .shm_header()
        .and_then(|h| h.get_selected_mic_id())
        .filter(|id| self.state.capture_endpoints.iter().any(|d| &d.id == id));
      // Unchanged mic: no re-read, so the restored wish survives the first frame and replugs.
      self.win_audio.chatmix.use_mic(picked.clone());
      self.state.selected_mic_endpoint_id = picked;
      if let Some(id) = self.win_audio.chatmix.mic_device_id.clone() {
        self.win_audio.mic_stream.set_input_device(Some(id));
      }
      self.state.spatial_checked_at = None;
    }
    super::header::refresh_spatial(&mut self.state, &mut self.win_audio);

    // Echo reference follows the Master output (the stream reopens only when it changes).
    self.win_audio.mic_stream.set_reference_device(self.win_audio.chatmix.master_device_id.clone());

    // Windows 11 Settings layout: slim top bar, navigation on the left, page on the right.
    // Mica-like flat chrome: no separator lines, the panels share the window background.
    let bg = ctx.global_style().visuals.panel_fill;
    let chrome = egui::Frame::NONE.fill(bg);
    egui::Panel::top("top_bar")
      .show_separator_line(false)
      .frame(chrome.inner_margin(egui::Margin::symmetric(8, 10)))
      .show(ui, |ui| {
        render_top_bar(ui, &mut self.state);
      });
    // Setup problems are shown right away on every tab, not hidden in the Mic tab.
    super::apo_banner::refresh(&mut self.state, &self.win_audio);
    super::setup::refresh(&mut self.state, &self.win_audio);
    let mic_warning = super::apo_banner::has_mic_warning(&self.state);
    let setup = super::setup::visible(&self.state);
    let problems = super::apo_banner::has_problems(&self.state);
    if mic_warning || setup || problems {
      egui::Panel::top("health_bar")
        .show_separator_line(false)
        .frame(chrome.inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 4 }))
        .show(ui, |ui| {
          // Most urgent first: someone is talking into a muted mic right now.
          if mic_warning {
            super::apo_banner::render_mic_warning(ui, &mut self.state);
          }
          if setup {
            super::setup::render(ui, &mut self.state);
          }
          if problems {
            super::apo_banner::render_health_bar(ui, &mut self.state, &self.win_audio);
          }
        });
    }
    egui::Panel::left("nav")
      .resizable(false)
      .exact_size(NAV_WIDTH)
      .show_separator_line(false)
      .frame(chrome.inner_margin(egui::Margin { left: 8, right: 8, top: 4, bottom: 8 }))
      .show(ui, |ui| {
        render_nav(ui, &mut self.state);
      });
    let page = chrome.inner_margin(egui::Margin { left: 24, right: 24, top: 8, bottom: 8 });
    egui::CentralPanel::default().frame(page).show(ui, |ui| {
      match self.state.active_tab {
        // The mixer strips fill the window height (it scrolls itself); so does the mic page.
        ActiveTab::Mixer => render_tab_mixer(ui, &mut self.state, &mut self.win_audio),
        ActiveTab::Mic => render_tab_mic(ui, &mut self.state, &mut self.win_audio),
        ActiveTab::Log => super::tab_log::render_tab_log(ui, &mut self.state),
        tab => {
          // Own scroll state per page: with egui's default id all pages shared one, and the
          // Log page's "follow" offset (f32::MAX) left the other pages scrolled past their content.
          egui::ScrollArea::vertical().id_salt(("page", tab)).auto_shrink([false, false]).show(ui, |ui| match tab {
            ActiveTab::Headphones => super::tab_headphones::render_tab_headphones(ui, &mut self.state, &mut self.win_audio),
            ActiveTab::Settings => super::tab_settings::render_tab_settings(ui, &mut self.state),
            other => render_tab_eq(ui, &mut self.state, other),
          });
        }
      }
    });

    // 4. Apply volume adjustments to live Windows audio sessions/endpoints & sync SHM.
    // Per-app session volumes belong to the background service while it runs.
    self.win_audio.chatmix.sessions_enabled = !self.state.is_daemon_active();
    self.win_audio.chatmix.apply_mix();
    self.state.sync_mixer_to_shm(&self.win_audio.chatmix);
    self.state.save_audio_settings_if_changed(ctx.input(|i| i.pointer.any_down()));

    // Continuous repaint for smooth meters while visible; minimized (or behind a fullscreen
    // game, which minimizes it) a slow tick is enough.
    ctx.request_repaint_after(if minimized { MINIMIZED_FRAME_INTERVAL } else { FRAME_INTERVAL });
    self.window.observe(ctx);
  }

  fn on_exit(&mut self) {
    self.window.save();
  }
}
