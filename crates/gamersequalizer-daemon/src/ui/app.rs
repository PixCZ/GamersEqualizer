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

pub struct GamersEqualizerApp {
  pub state: AppState,
  pub win_audio: WinAudioSystem,
  applied_mica: bool,
  window: super::window_state::WindowTracker,
  daemon_checked_at: Instant,
}

impl GamersEqualizerApp {
  pub fn new(state: AppState, win_audio: WinAudioSystem, window: super::window_state::WindowTracker) -> Self {
    Self { state, win_audio, applied_mica: false, window, daemon_checked_at: Instant::now() }
  }
}

impl eframe::App for GamersEqualizerApp {
  fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    if !self.applied_mica {
      self.applied_mica = true;
    }

    // 0. The background service carries the echo reference and the mic telemetry. Restart it
    // if it died (or was still exiting when this window started, so the startup check missed it).
    if self.daemon_checked_at.elapsed() >= DAEMON_CHECK_INTERVAL {
      self.daemon_checked_at = Instant::now();
      crate::ensure_daemon_running();
    }

    // 1. Synchronize system master and mic volume from Windows in real time. A mic level the
    // service's automatic input gain set is not the user's volume.
    if let Some(h) = self.state.shm_header() {
      let auto = h.mic_auto_gain_permille.load(std::sync::atomic::Ordering::Relaxed);
      self.win_audio.chatmix.mic_hw_auto = (auto > 0).then(|| auto as f32 / 1000.0);
    }
    if self.win_audio.chatmix.sync_from_system() {
      self.state.sync_mixer_to_shm(&self.win_audio.chatmix);
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
    let dev_changed = self.win_audio.endpoint_lock.poll_device_changed();
    let current_sys_default = self.win_audio.endpoint_lock.get_default_output_id();
    let current_sys_comms = self.win_audio.endpoint_lock.get_default_communications_id();
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
      if let Some(ref id) = picked {
        self.win_audio.chatmix.mic_device_id = Some(id.clone());
      }
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
    let bg = ctx.style().visuals.panel_fill;
    let chrome = egui::Frame::none().fill(bg);
    egui::TopBottomPanel::top("top_bar")
      .show_separator_line(false)
      .frame(chrome.inner_margin(egui::Margin::symmetric(8.0, 10.0)))
      .show(ctx, |ui| {
        render_top_bar(ui, &mut self.state);
      });
    // Setup problems are shown right away on every tab, not hidden in the Mic tab.
    super::apo_banner::refresh(&mut self.state, &self.win_audio);
    super::setup::refresh(&mut self.state, &self.win_audio);
    let mic_warning = super::apo_banner::has_mic_warning(&self.state);
    let setup = super::setup::visible(&self.state);
    let problems = super::apo_banner::has_problems(&self.state);
    if mic_warning || setup || problems {
      egui::TopBottomPanel::top("health_bar")
        .show_separator_line(false)
        .frame(chrome.inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 0.0, bottom: 4.0 }))
        .show(ctx, |ui| {
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
    egui::SidePanel::left("nav")
      .resizable(false)
      .exact_width(NAV_WIDTH)
      .show_separator_line(false)
      .frame(chrome.inner_margin(egui::Margin { left: 8.0, right: 8.0, top: 4.0, bottom: 8.0 }))
      .show(ctx, |ui| {
        render_nav(ui, &mut self.state);
      });
    let page = chrome.inner_margin(egui::Margin { left: 24.0, right: 24.0, top: 8.0, bottom: 8.0 });
    egui::CentralPanel::default().frame(page).show(ctx, |ui| {
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

    // 4. Apply volume adjustments to live Windows audio sessions/endpoints & sync SHM
    self.win_audio.chatmix.apply_mix();
    self.state.sync_mixer_to_shm(&self.win_audio.chatmix);
    self.state.save_audio_settings_if_changed(ctx.input(|i| i.pointer.any_down()));

    // Request continuous repaint while window is visible for smooth meters
    ctx.request_repaint_after(std::time::Duration::from_millis(30));
    self.window.observe(ctx);
  }

  fn on_exit(&mut self) {
    self.window.save();
  }
}
