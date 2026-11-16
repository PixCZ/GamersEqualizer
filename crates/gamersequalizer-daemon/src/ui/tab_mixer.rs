use super::state::{ActiveTab, AppState};
use super::theme::{self, Icon, icons};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::app_channels::{self, AppChannel};
use crate::win_audio::{AudioBus, AudioSessionInfo, WinAudioSystem};
use egui::{Color32, CornerRadius, RichText, Sense, Ui, Vec2};
use gamersequalizer_common::streams::StreamBus;
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// Smallest height of the app list in each channel strip; it grows (faders shrink) with more apps.
const APPS_LIST_HEIGHT: f32 = 84.0;
/// One stacked app chip including the gap below it.
const APP_ROW_HEIGHT: f32 = 24.0;
/// Vertical fader height range; it grows with the window.
const FADER_MIN: f32 = 110.0;
const FADER_MAX: f32 = 260.0;
/// Everything on the page except the faders (header, strip parts, ChatMix).
const PAGE_WITHOUT_FADERS: f32 = 480.0;
const MASTER_COLOR: Color32 = Color32::from_rgb(78, 168, 222);
const GAME_COLOR: Color32 = Color32::from_rgb(82, 183, 136);
const CHAT_COLOR: Color32 = Color32::from_rgb(72, 202, 228);
const MEDIA_COLOR: Color32 = Color32::from_rgb(247, 37, 133);
const MIC_COLOR: Color32 = Color32::from_rgb(247, 127, 0);
/// How often the mic strip re-reads which apps are recording (a registry walk; the page
/// repaints every frame).
const RECORDING_REFRESH: Duration = Duration::from_secs(1);
const SESSIONS_REFRESH: Duration = Duration::from_millis(500);

/// One app (all its processes) in a channel's apps box.
struct AppItem {
  exe: String,
  processes: usize,
  /// The user dragged it here (vs. the built-in lists).
  pinned: bool,
}

/// (exe, new channel or None = back to automatic) picked in the mixer this frame.
type AppMove = Option<(String, Option<AppChannel>)>;

/// What a strip's apps box shows.
enum AppsBox<'a> {
  /// Read-only app names (master: everything playing; mic: apps recording).
  List { caption: &'a str, empty: &'a str, apps: &'a [String], hover: Option<&'a str> },
  /// Apps on this channel: each can be dragged to another channel strip, and this box
  /// accepts drops.
  Channel { channel: AppChannel, apps: &'a [AppItem], moved: &'a mut AppMove },
}

/// Fixed look of one channel strip.
struct Strip<'a> {
  title: &'a str,
  icon: Icon,
  color: Color32,
  preset: &'a str,
  max_vol: f32,
  fader_h: f32,
  /// Height of the apps list (same in every strip so they line up).
  list_h: f32,
  meter: Option<f32>,
  footers: &'a FooterSync,
}

/// Footers differ in height (device dropdown vs. a caption line). Each strip's apps list grows
/// by what its footer is shorter than the tallest one, so all strips end at the same line.
/// Heights are measured while drawing, so this uses last frame's (the page repaints anyway).
struct FooterSync {
  /// Tallest footer last frame.
  tallest: f32,
  /// Tallest footer so far this frame.
  tallest_now: Cell<f32>,
}

pub fn render_tab_mixer(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  // The strips fill the window height (measured here, outside the scroll area). Once the faders
  // are at their minimum (small monitors) the rest scrolls, so ChatMix stays reachable.
  let page_h = ui.available_height();
  egui::ScrollArea::vertical()
    .id_salt("mixer_page")
    .auto_shrink([false, false])
    .show(ui, |ui| mixer_page(ui, state, win_audio, page_h));
}

fn mixer_page(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem, page_h: f32) {
  let sessions = sessions_cached(&win_audio.chatmix);

  let game_apps = apps_on(&sessions, AudioBus::Game);
  let chat_apps = apps_on(&sessions, AudioBus::Chat);
  let media_apps = apps_on(&sessions, AudioBus::Media);
  let mut all_apps: Vec<String> =
    game_apps.iter().chain(&chat_apps).chain(&media_apps).map(|a| a.exe.clone()).collect();
  all_apps.sort_by_key(|a| a.to_ascii_lowercase());
  let recording = recording_apps_cached();

  // A longer app list takes its height from the faders. Only when they are at their minimum
  // does the list scroll (and the page, if the window is still too short). Master's list (everything) is left out,
  // it would always win; it scrolls within the shared height.
  let rows = [game_apps.len(), chat_apps.len(), media_apps.len(), recording.len()].into_iter().max().unwrap_or(0);
  let wanted_list = (rows as f32 * APP_ROW_HEIGHT).max(APPS_LIST_HEIGHT);
  let room = page_h - PAGE_WITHOUT_FADERS;
  let fader_h = (room - (wanted_list - APPS_LIST_HEIGHT)).clamp(FADER_MIN, FADER_MAX);
  let list_h = wanted_list.min(APPS_LIST_HEIGHT + (room - fader_h).max(0.0));
  let mut moved: AppMove = None;
  let tallest_id = egui::Id::unique("mixer_tallest_footer");
  let footers = FooterSync { tallest: ui.data(|d| d.get_temp(tallest_id)).unwrap_or(0.0), tallest_now: Cell::new(0.0) };

  theme::page_header(
    ui,
    icons::VOLUME,
    MASTER_COLOR,
    tr("Mixer & device"),
    Some(tr("Volume per app group, where each one plays and the game/chat balance.")),
  );

  // 5 Proportional Columns filling exact window width
  let mut jump_tab = None;

  ui.columns(5, |cols| {
    for col in cols.iter_mut() {
      col.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
    }
    let [master_col, game_col, chat_col, media_col, mic_col] = cols else { return };
    // Footers name the device each channel really plays on.
    let output_of = |id: Option<&String>, fallback: &str| {
      let name = id.and_then(|id| state.endpoints.iter().find(|e| &e.id == id)).map(|e| e.name.clone());
      trf("Output: {}", &[&name.unwrap_or_else(|| fallback.to_string())])
    };
    let game_tag = output_of(win_audio.chatmix.game_device_id.as_ref(), tr("same as Master"));
    let media_tag = output_of(win_audio.chatmix.media_device_id.as_ref(), tr("same as Master"));

    // 1. MASTER
    let mut picked_output = None;
    let strip = Strip {
      title: tr("Master"),
      icon: icons::VOLUME,
      color: MASTER_COLOR,
      preset: state.output_eq.headphone_profile_name().unwrap_or(tr("No headphone profile")),
      max_vol: 1.0,
      fader_h,
      list_h,
      footers: &footers,
      meter: None,
    };
    render_strip(
      master_col,
      &strip,
      &mut win_audio.chatmix.master.volume,
      &mut win_audio.chatmix.master.muted,
      AppsBox::List { caption: tr("Playing on all channels"), empty: tr("None active"), apps: &all_apps, hover: None },
      || jump_tab = Some(ActiveTab::Headphones),
      |ui: &mut Ui| {
        let width = ui.available_width() - 28.0;
        let chars = ((width - 20.0) / 6.5).max(6.0) as usize;
        picked_output = super::header::output_combo(ui, state, "master_output_dropdown", width, chars);
      },
    );

    if let Some(id) = picked_output {
      super::header::select_output(state, win_audio, id);
    }

    // 2. GAME
    let strip = Strip {
      title: tr("Game"),
      icon: icons::GAME,
      color: GAME_COLOR,
      preset: state.output_eq.game_profile_name().map_or(tr("No game profile"), tr),
      max_vol: 1.0,
      fader_h,
      list_h,
      footers: &footers,
      meter: None,
    };
    render_strip(
      game_col,
      &strip,
      &mut win_audio.chatmix.game.volume,
      &mut win_audio.chatmix.game.muted,
      AppsBox::Channel { channel: AppChannel::Game, apps: &game_apps, moved: &mut moved },
      || jump_tab = Some(ActiveTab::Game),
      |ui: &mut Ui| footer_tag(ui, &game_tag),
    );

    // 3. CHAT
    let selected_chat_ep = state.selected_chat_endpoint_id.clone();
    let preset = |bus| state.output_eq.bus_profile_name(bus).unwrap_or(tr("Flat")).to_string();
    let (chat_preset, media_preset) = (preset(StreamBus::Chat), preset(StreamBus::Media));
    let endpoints = state.endpoints.clone();
    let primary_def = state.selected_endpoint_id.clone();
    let mut changed_chat_route = None;

    let strip = Strip {
      title: tr("Chat"),
      icon: icons::CHAT,
      color: CHAT_COLOR,
      preset: &chat_preset,
      max_vol: 1.0,
      fader_h,
      list_h,
      footers: &footers,
      meter: None,
    };
    render_strip(
      chat_col,
      &strip,
      &mut win_audio.chatmix.chat.volume,
      &mut win_audio.chatmix.chat.muted,
      AppsBox::Channel { channel: AppChannel::Chat, apps: &chat_apps, moved: &mut moved },
      || jump_tab = Some(ActiveTab::Chat),
      |ui: &mut Ui| {
        let current_route_label = if let Some(ref cid) = selected_chat_ep {
          endpoints
            .iter()
            .find(|e| &e.id == cid)
            .map(|e| super::truncate_chars(&e.name, 18))
            .unwrap_or_else(|| tr("Virtual cable").to_string())
        } else {
          tr("Direct (Master)").to_string()
        };

        egui::ComboBox::from_id_salt("chat_route_dropdown")
          .selected_text(RichText::new(fit(ui, &trf("Route: {}", &[&current_route_label]))).small())
          .width(ui.available_width() - 28.0)
          .show_ui(ui, |ui| {
            let direct = theme::with_icon(icons::HEADPHONES, tr("Direct (same as Master)"));
            if ui.selectable_label(selected_chat_ep.is_none(), direct).clicked() {
              changed_chat_route = Some(None);
            }
            for ep in &endpoints {
              let is_sel = selected_chat_ep.as_deref() == Some(&ep.id);
              if ui.selectable_label(is_sel, &ep.name).clicked() {
                changed_chat_route = Some(Some(ep.id.clone()));
              }
            }
          });
      },
    );

    if let Some(new_route) = changed_chat_route {
      state.selected_chat_endpoint_id = new_route.clone();
      if let Some(target_id) = new_route {
        let _ = win_audio.endpoint_lock.set_as_communications_device(&target_id);
      } else if let Some(ref def_id) = primary_def {
        let _ = win_audio.endpoint_lock.set_as_communications_device(def_id);
      }
    }

    // 4. MEDIA
    let strip = Strip {
      title: tr("Media"),
      icon: icons::MEDIA,
      color: MEDIA_COLOR,
      preset: &media_preset,
      max_vol: 1.0,
      fader_h,
      list_h,
      footers: &footers,
      meter: None,
    };
    render_strip(
      media_col,
      &strip,
      &mut win_audio.chatmix.media.volume,
      &mut win_audio.chatmix.media.muted,
      AppsBox::Channel { channel: AppChannel::Media, apps: &media_apps, moved: &mut moved },
      || jump_tab = Some(ActiveTab::Media),
      |ui: &mut Ui| footer_tag(ui, &media_tag),
    );

    // 5. MIC
    let selected_mic_ep = state.selected_mic_endpoint_id.clone();
    let capture_endpoints = state.capture_endpoints.clone();
    let mut changed_mic_ep = None;

    let strip = Strip {
      title: tr("Mic"),
      icon: icons::MIC,
      color: MIC_COLOR,
      preset: tr(&state.mic_profile.name),
      max_vol: 3.0,
      fader_h,
      list_h,
      footers: &footers,
      meter: Some(win_audio.chatmix.mic_peak_level),
    };
    render_strip(
      mic_col,
      &strip,
      &mut win_audio.chatmix.mic.volume,
      &mut win_audio.chatmix.mic.muted,
      AppsBox::List {
        caption: tr("Recording now"),
        empty: tr("No app is recording"),
        apps: &recording,
        hover: Some(tk("{} - recording from a microphone (as Windows reports it).")),
      },
      || jump_tab = Some(ActiveTab::Mic),
      |ui: &mut Ui| {
        let current_mic_label = if let Some(ref mid) = selected_mic_ep {
          capture_endpoints
            .iter()
            .find(|e| &e.id == mid)
            .map(|e| super::truncate_chars(&e.name, 18))
            .unwrap_or_else(|| tr("Microphone").to_string())
        } else {
          tr("Default mic").to_string()
        };

        egui::ComboBox::from_id_salt("mic_input_dropdown")
          .selected_text(RichText::new(fit(ui, &trf("In: {}", &[&current_mic_label]))).small())
          .width(ui.available_width() - 28.0)
          .show_ui(ui, |ui| {
            if ui.selectable_label(selected_mic_ep.is_none(), tr("Default mic")).clicked() {
              changed_mic_ep = Some(None);
            }
            for ep in &capture_endpoints {
              let is_sel = selected_mic_ep.as_deref() == Some(&ep.id);
              if ui.selectable_label(is_sel, &ep.name).clicked() {
                changed_mic_ep = Some(Some(ep.id.clone()));
              }
            }
          });
      },
    );

    if let Some(pick) = changed_mic_ep {
      super::tab_mic::pick_mic(state, win_audio, pick);
    }
  });

  ui.data_mut(|d| d.insert_temp(tallest_id, footers.tallest_now.get()));

  if let Some(tab) = jump_tab {
    state.active_tab = tab;
  }
  // Volumes follow at the end of this frame (apply_mix); the chat/media EQ pairing within
  // ~1 s (the daemon re-reads the file).
  if let Some((exe, channel)) = moved {
    log::info!("[Mixer] {exe} -> {}", channel.map_or("automatic".to_string(), |c| format!("{c:?}")));
    app_channels::set(&exe, channel);
  }

  ui.add_space(2.0);
  ui.label(theme::caption(tr("Drag an app to another channel to move it there; right-click it to undo.")));
  ui.add_space(theme::CARD_GAP);

  // ChatMix balance card
  theme::card(ui, |ui| {
    ui.horizontal(|ui| {
      ui.label(theme::strong(tr("ChatMix"), 16.0));
      ui.add_space(12.0);
      let game = theme::adapt(ui, GAME_COLOR);
      theme::icon_label(ui, icons::GAME, 16.0, game, RichText::new(tr("Game")).color(game));

      let mut balance = win_audio.chatmix.balance;
      let slider = egui::Slider::new(&mut balance, -1.0..=1.0).show_value(false);
      let width = (ui.available_width() - 470.0).clamp(140.0, 520.0);
      ui.spacing_mut().slider_width = width;
      let r = ui.add(slider).on_hover_text(if balance < -0.05 {
        trf("Game +{}%", &[&format!("{:.0}", -balance * 100.0)])
      } else if balance > 0.05 {
        trf("Chat +{}%", &[&format!("{:.0}", balance * 100.0)])
      } else {
        tr("Equal (50/50)").to_string()
      });
      if r.changed() {
        win_audio.chatmix.set_balance(balance);
      }

      let chat = theme::adapt(ui, CHAT_COLOR);
      theme::label_icon(ui, RichText::new(tr("Chat")).color(chat), icons::CHAT, 16.0, chat);

      let (g_mult, c_mult) = win_audio.chatmix.get_multipliers();
      ui.label(
        RichText::new(format!("{:.0} % / {:.0} %", g_mult * 100.0, c_mult * 100.0))
          .color(theme::palette(ui).text_secondary),
      );

      if ui.button(tr("Center (50/50)")).clicked() {
        win_audio.chatmix.set_balance(0.0);
      }
    });
  });

  // Always apply volume adjustments to WASAPI sessions and sync with background daemon
  win_audio.chatmix.apply_mix();
  state.sync_mixer_to_shm(&win_audio.chatmix);
}

fn render_strip(
  ui: &mut Ui,
  strip: &Strip,
  volume: &mut f32,
  is_muted: &mut bool,
  apps: AppsBox,
  on_preset_click: impl FnOnce(),
  footer: impl FnOnce(&mut Ui),
) {
  let p = theme::palette(ui);
  let color = theme::adapt(ui, strip.color);
  egui::Frame::NONE
    .fill(p.card)
    .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
    .stroke(egui::Stroke::new(1.0_f32, p.card_stroke))
    .inner_margin(egui::Margin::same(12))
    .show(ui, |ui| {
      ui.set_width(ui.available_width());
      // 1. Channel title
      theme::icon_label(ui, strip.icon, 16.0, color, theme::strong(strip.title, 15.0));

      // 2. Preset pill (link to the channel's EQ page)
      let preset = super::truncate_chars(strip.preset, 15);
      if ui
        .add_sized(
          [ui.available_width(), 26.0],
          egui::Button::new(RichText::new(format!("{preset}  ↗")).small().color(color)),
        )
        .on_hover_text(trf("Click to customize EQ curves for {}", &[&strip.title]))
        .clicked()
      {
        on_preset_click();
      }

      ui.add_space(4.0);

      // 3. Fader, gain readout and live meter
      ui.vertical_centered(|ui| {
        // A vertical slider's length comes from slider_width (theme: 200), not from add_sized,
        // so set it or the fader ignores fader_h while the meter next to it uses it.
        ui.spacing_mut().slider_width = strip.fader_h;
        let fader = egui::Slider::new(volume, 0.0..=strip.max_vol).vertical().show_value(false);
        if let Some(peak) = strip.meter {
          ui.horizontal(|ui| {
            let total_w = 26.0 + 6.0 + 6.0;
            ui.add_space(((ui.available_width() - total_w) / 2.0).max(0.0));
            ui.add_sized(Vec2::new(26.0, strip.fader_h), fader);
            ui.add_space(6.0);
            peak_meter(ui, peak, strip.fader_h);
          });
        } else {
          ui.add_sized(Vec2::new(26.0, strip.fader_h), fader);
        }

        let vol_pct = if *volume <= 1.0 {
          format!("{:.0} %", *volume * 100.0)
        } else {
          format!("{:.0} % (+{:.1} dB)", *volume * 100.0, 20.0 * volume.log10())
        };
        ui.label(theme::strong(vol_pct, 15.0));

        let (mute_text, mute_color) = if *is_muted {
          (theme::with_icon(icons::MUTE, tr("Muted")), p.err)
        } else {
          (theme::with_icon(icons::VOLUME, tr("On")), p.text)
        };
        if ui
          .add_sized(
            [ui.available_width().min(120.0), 28.0],
            egui::Button::new(RichText::new(mute_text).color(mute_color)),
          )
          .clicked()
        {
          *is_muted = !*is_muted;
        }
      });

      ui.add_space(4.0);

      // 4. Apps box
      // ui.columns lays out justified, which would stretch wrapped labels word by word.
      let footer_id = ui.scope_id().with(("strip_footer", strip.title));
      let own_footer: f32 = ui.data(|d| d.get_temp(footer_id)).unwrap_or(strip.footers.tallest);
      let list_h = strip.list_h + (strip.footers.tallest - own_footer).max(0.0);
      ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        render_apps_box(ui, strip.title, color, list_h, apps)
      });
      let footer_top = ui.min_rect().bottom();
      footer(ui);
      let footer_h = ui.min_rect().bottom() - footer_top;
      ui.data_mut(|d| d.insert_temp(footer_id, footer_h));
      strip.footers.tallest_now.set(strip.footers.tallest_now.get().max(footer_h));
    });
}

/// One-line footer cut to the strip width; the full text on hover.
fn footer_tag(ui: &mut Ui, text: &str) {
  ui.label(theme::caption(fit(ui, text))).on_hover_text(text);
}

/// Shortens a combo label to the strip width: egui grows the combo with its text, which
/// pushed the strip out of its column.
fn fit(ui: &Ui, text: &str) -> String {
  // ~6.5 pt per character of the 12 pt caption font, minus the arrow and padding.
  super::truncate_chars(text, ((ui.available_width() - 48.0) / 6.5).max(6.0) as usize)
}

/// Thin vertical peak meter next to the mic fader (green / amber / red).
fn peak_meter(ui: &mut Ui, peak: f32, height: f32) {
  let (rect, _resp) = ui.allocate_exact_size(Vec2::new(6.0, height), Sense::hover());
  let p = theme::palette(ui);
  let painter = ui.painter();
  painter.rect_filled(rect, CornerRadius::same(3), p.inset);
  let level = peak.clamp(0.0, 1.0);
  if level > 0.005 {
    let fill_h = rect.height() * level;
    let fill_rect = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - fill_h), rect.max);
    let color = if level > 0.88 {
      Color32::from_rgb(235, 75, 75)
    } else if level > 0.70 {
      Color32::from_rgb(245, 190, 35)
    } else {
      Color32::from_rgb(46, 204, 113)
    };
    painter.rect_filled(fill_rect, CornerRadius::same(3), color);
  }
}

/// Apps recording from a microphone, re-read at most every `RECORDING_REFRESH` (UI thread only).
pub(super) fn recording_apps_cached() -> Vec<String> {
  thread_local! {
    static CACHE: RefCell<Option<(Instant, Vec<String>)>> = const { RefCell::new(None) };
  }
  CACHE.with_borrow_mut(|cache| {
    if cache.as_ref().is_none_or(|(at, _)| at.elapsed() >= RECORDING_REFRESH) {
      *cache = Some((Instant::now(), crate::mic_usage::recording_apps()));
    }
    cache.as_ref().map(|(_, apps)| apps.clone()).unwrap_or_default()
  })
}

/// App sessions, re-enumerated at most every `SESSIONS_REFRESH` (UI thread only): a full pass
/// opens every process with a session, too much for every frame.
fn sessions_cached(chatmix: &crate::win_audio::ChatMixController) -> Vec<AudioSessionInfo> {
  thread_local! {
    static CACHE: RefCell<Option<(Instant, Vec<AudioSessionInfo>)>> = const { RefCell::new(None) };
  }
  CACHE.with_borrow_mut(|cache| {
    if cache.as_ref().is_none_or(|(at, _)| at.elapsed() >= SESSIONS_REFRESH) {
      *cache = Some((Instant::now(), chatmix.list_active_sessions()));
    }
    cache.as_ref().map(|(_, s)| s.clone()).unwrap_or_default()
  })
}

/// Apps of `bus`, one entry per exe (processes counted), sorted by name.
fn apps_on(sessions: &[AudioSessionInfo], bus: AudioBus) -> Vec<AppItem> {
  let mut apps: Vec<AppItem> = Vec::new();
  for s in sessions.iter().filter(|s| s.bus == bus) {
    match apps.iter_mut().find(|a| a.exe.eq_ignore_ascii_case(&s.exe_name)) {
      Some(app) => app.processes += 1,
      None => {
        apps.push(AppItem { exe: s.exe_name.clone(), processes: 1, pinned: app_channels::get(&s.exe_name).is_some() })
      }
    }
  }
  apps.sort_by_key(|a| a.exe.to_ascii_lowercase());
  apps
}

fn render_apps_box(ui: &mut Ui, title: &str, color: Color32, list_h: f32, apps: AppsBox) {
  let frame = theme::inset_frame(ui);
  // Scrolls only when the list outgrew the height the faders could give up.
  let list_area = || {
    egui::ScrollArea::vertical()
      .id_salt(("mixer_apps", title))
      .min_scrolled_height(list_h)
      .max_height(list_h)
      .auto_shrink([false, false])
  };
  match apps {
    AppsBox::List { caption, empty, apps, hover } => {
      frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::caption(caption));
        list_area().show(ui, |ui| {
          ui.spacing_mut().item_spacing.y = 4.0;
          if apps.is_empty() {
            ui.label(theme::caption(empty));
          }
          for app in apps {
            // Plain text, no chip: these can't be dragged.
            let line = ui.label(RichText::new(super::truncate_chars(app, 20)).small());
            match hover {
              Some(text) => line.on_hover_text(trf(text, &[app])),
              None => line.on_hover_text(app),
            };
          }
        });
      });
    }
    AppsBox::Channel { channel, apps, moved } => {
      let (_, dropped) = ui.dnd_drop_zone::<String, ()>(frame, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::caption(tr("Apps")));
        list_area().show(ui, |ui| {
          ui.spacing_mut().item_spacing.y = 4.0;
          if apps.is_empty() {
            ui.label(theme::caption(tr("None active - drop an app here")));
          }
          // One chip per line, never wrapping its text: the name is cut to the line width,
          // ~6 pt per small-font character minus the chip padding.
          let max_chars = ((ui.available_width() - 20.0) / 6.0).max(6.0) as usize;
          for app in apps {
            app_chip(ui, app, color, max_chars, moved);
          }
        });
      });
      if let Some(exe) = dropped
        && !apps.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe))
      {
        *moved = Some(((*exe).clone(), Some(channel)));
      }
    }
  }
}

/// One draggable app, drawn as a chip in the channel's color (the chip says "grab me");
/// right-click offers going back to the automatic channel.
fn app_chip(ui: &mut Ui, app: &AppItem, color: Color32, max_chars: usize, moved: &mut AppMove) {
  let mut suffix = String::new();
  if app.processes > 1 {
    suffix = format!(" ×{}", app.processes);
  }
  let pin = if app.pinned { "📌 " } else { "" };
  let room = max_chars.saturating_sub(pin.chars().count() + suffix.chars().count()).max(4);
  let text = format!("{pin}{}{suffix}", super::truncate_chars(&app.exe, room));
  let chip = egui::Frame::NONE
    .fill(color.gamma_multiply(0.18))
    .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
    .corner_radius(CornerRadius::same(10))
    .inner_margin(egui::Margin::symmetric(7, 2))
    .show(ui, |ui| {
      // Full line width: the chips stack into a tidy column and are easy to grab.
      ui.set_width(ui.available_width());
      ui.add(egui::Label::new(RichText::new(text).small()).wrap_mode(egui::TextWrapMode::Extend).selectable(false))
    });
  let response =
    ui.interact(chip.response.rect, ui.scope_id().with(("mixer_app_chip", &app.exe)), Sense::click_and_drag());
  response.dnd_set_drag_payload(app.exe.clone());
  if response.dragged() {
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    // Drag ghost next to the pointer.
    if let Some(pos) = ui.ctx().pointer_interact_pos() {
      let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("mixer_app_drag"));
      ui.ctx().layer_painter(layer).text(
        pos + Vec2::new(12.0, 4.0),
        egui::Align2::LEFT_TOP,
        &app.exe,
        egui::FontId::proportional(13.0),
        theme::palette(ui).text,
      );
    }
  } else if response.hovered() {
    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
  }
  let hover = if app.pinned {
    trf("{} - you put it on this channel. Drag to move, right-click to undo.", &[&app.exe])
  } else {
    trf("{} - drag to another channel to move it.", &[&app.exe])
  };
  response.on_hover_text(hover).context_menu(|ui| {
    if ui.add_enabled(app.pinned, egui::Button::new(tr("Back to automatic channel"))).clicked() {
      *moved = Some((app.exe.clone(), None));
      ui.close();
    }
  });
}
