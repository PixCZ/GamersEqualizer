use super::state::AppState;
use super::tab_mic::EQ_COLOR;
use super::theme::{self, icons, Severity};
use crate::i18n::{tr, trf};
use crate::win_audio::apo_install::ApoStatus;
use crate::win_audio::sidetone;
use crate::win_audio::WinAudioSystem;
use egui::{Color32, RichText, Ui};
use gamersequalizer_common::biquad::MAX_EQ_BANDS;
use gamersequalizer_common::sidetone::{
  device_hash, NoiseMode, SidetonePair, GATE_MARGIN_DB_MAX, GATE_MARGIN_DB_MIN, GATE_RANGE_DB_MAX, VOLUME_MAX,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(247, 127, 0);

pub fn render_tab_sidetone(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  theme::page_header(
    ui,
    icons::HEADPHONES,
    ACCENT,
    tr("Sidetone"),
    Some(tr("Sealed headphones block your own voice. Sidetone plays the microphone back into them.")),
  );

  // 1. Resolve active endpoints and names
  let mic_id = state.selected_mic_endpoint_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
  let out_id = win_audio.endpoint_lock.get_default_output_id().or_else(|| state.selected_endpoint_id.clone());

  let mic_name = mic_id
    .as_deref()
    .and_then(|id| state.capture_endpoints.iter().find(|e| e.id == id).map(|e| e.name.clone()))
    .unwrap_or_else(|| tr("Unknown device").to_string());

  let out_name = out_id
    .as_deref()
    .and_then(|id| state.endpoints.iter().find(|e| e.id == id).map(|e| e.name.clone()))
    .unwrap_or_else(|| tr("Unknown device").to_string());

  let pair_key = (mic_id.unwrap_or_default(), out_id.unwrap_or_default());

  // 2. Load or initialize the active pair
  let (saved_pairs, _) = sidetone::pairs();
  if state.sidetone_ui.pair.is_none() || state.sidetone_ui.pair_key != pair_key {
    let mut pair = saved_pairs
      .find(&pair_key.0, &pair_key.1)
      .cloned()
      .unwrap_or_else(|| SidetonePair::new(&pair_key.0, &pair_key.1, &mic_name, &out_name));
    pair.mic_name = mic_name.clone();
    pair.output_name = out_name.clone();
    state.sidetone_ui.pair = Some(pair);
    state.sidetone_ui.pair_key = pair_key.clone();
    state.sidetone_ui.dirty = false;
  }

  // 3. Track beats and update state
  let (reader_beat, writer_beat) = state.shm_header().map_or((0, 0), |h| {
    let r = Ordering::Relaxed;
    (h.sidetone_reader_beat.load(r), h.sidetone_writer_beat.load(r))
  });
  let now = Instant::now();
  if reader_beat != state.sidetone_ui.last_reader_beat {
    state.sidetone_ui.last_reader_beat = reader_beat;
    state.sidetone_ui.last_reader_beat_time = Some(now);
  }
  if writer_beat != state.sidetone_ui.last_writer_beat {
    state.sidetone_ui.last_writer_beat = writer_beat;
    state.sidetone_ui.last_writer_beat_time = Some(now);
  }
  let reader_running =
    state.sidetone_ui.last_reader_beat_time.is_some_and(|t| now.duration_since(t) < Duration::from_secs(1));
  let writer_running =
    state.sidetone_ui.last_writer_beat_time.is_some_and(|t| now.duration_since(t) < Duration::from_secs(1));

  if state.sidetone_ui.testing {
    if let Some(h) = state.shm_header() {
      h.sidetone_test_beat.fetch_add(1, Ordering::Relaxed);
    }
    ui.ctx().request_repaint_after(Duration::from_millis(200));
  }

  let mut pair = state.sidetone_ui.pair.clone().unwrap_or_default();
  pair.mic_name = mic_name;
  pair.output_name = out_name;
  let mut settings = pair.settings();
  let mut changed = false;
  let mut eq_changed = false;
  let mut enabled_toggled = false;

  // -------------------------------------------------------------------------------------
  // Card 1: Active pair, toggle, status & try-it
  // -------------------------------------------------------------------------------------
  let (mic_muted, out_hash_seen, out_hash) = state.shm_header().map_or((false, 0, 0), |h| {
    let r = Ordering::Relaxed;
    (h.mic_muted.load(r), h.sidetone_output_hash_seen.load(r), h.sidetone_output_hash.load(r))
  });

  theme::card(ui, |ui| {
    ui.horizontal(|ui| {
      theme::card_title(ui, Some((icons::HEADPHONES, ACCENT)), &trf("{} → {}", &[&pair.mic_name, &pair.output_name]));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if theme::toggle_labeled(ui, settings.enabled, tr("On for this pair")) {
          settings.enabled = !settings.enabled;
          changed = true;
          enabled_toggled = true;
          if !settings.enabled {
            state.sidetone_ui.testing = false;
          }
        }
      });
    });

    ui.add_space(theme::CARD_GAP);

    theme::info_bar(ui, Severity::Info, |ui| {
      ui.label(tr("Exclusive-mode playback bypasses Windows effects: sidetone is silent then."));
    });

    ui.add_space(4.0);

    // Status line per section 6.5
    let mic_check = super::apo_banner::mic_check(state);
    let mic_apo_missing = mic_check.is_some_and(|c| !matches!(c.status, ApoStatus::Active));
    let out_apo_missing = super::setup::output_needs_setup(state).is_some();

    if mic_apo_missing {
      super::apo_banner::render_apo_status_line(ui, state);
    } else if out_apo_missing {
      theme::status_line(ui, Severity::Warning, tr("Output EQ is not installed on this output device."));
    } else if !settings.enabled {
      theme::status_line(ui, Severity::Info, tr("Off for this pair."));
    } else if mic_muted {
      theme::status_line(ui, Severity::Warning, tr("The microphone is muted: sidetone is silent."));
    } else if out_hash_seen != 0 && out_hash_seen != out_hash && !reader_running {
      theme::status_line(
        ui,
        Severity::Info,
        &trf("Another output is playing: sidetone waits for {}.", &[&pair.output_name]),
      );
    } else if !writer_running && !state.sidetone_ui.testing {
      theme::status_line(ui, Severity::Info, tr("Waiting: no app is recording from this microphone."));
    } else if reader_running {
      theme::status_line(ui, Severity::Success, tr("Playing."));
    } else {
      theme::status_line(ui, Severity::Info, tr("Starting…"));
    }

    ui.add_space(8.0);

    // Try-it button
    ui.horizontal(|ui| {
      ui.add_enabled_ui(settings.enabled, |ui| {
        let btn_label = if state.sidetone_ui.testing { tr("Stop trying") } else { tr("Try it") };
        let btn =
          if state.sidetone_ui.testing { ui.button(btn_label) } else { ui.add(theme::accent_button(ui, btn_label)) };
        if btn
          .on_hover_text(tr("Plays without a recording app. Windows shows the microphone as in use meanwhile."))
          .clicked()
        {
          state.sidetone_ui.testing = !state.sidetone_ui.testing;
        }
      });
      ui.label(theme::caption(tr("Plays without a recording app. Windows shows the microphone as in use meanwhile.")));
    });

    // Latency report when reader is active
    if reader_running {
      if let Some(h) = state.shm_header() {
        let r = Ordering::Relaxed;
        let age_us = h.sidetone_age_us.load(r);
        let noise_delay_us = h.sidetone_noise_delay_us.load(r);
        let out_rate = h.sidetone_out_rate.load(r).max(1);
        let out_block_frames = h.sidetone_out_block_frames.load(r);
        let out_block_us = (out_block_frames as u64 * 1_000_000 / out_rate as u64) as u32;
        let latence_us = age_us.saturating_add(noise_delay_us).saturating_add(out_block_us);
        let delay_ms = latence_us as f32 / 1000.0;
        let p = theme::palette(ui);
        let col = if delay_ms <= 10.0 {
          p.ok
        } else if delay_ms <= 20.0 {
          p.warn
        } else {
          p.err
        };
        ui.add_space(8.0);
        ui.label(
          RichText::new(trf("Delay about {} ms (without the converters)", &[&format!("{:.0}", delay_ms.round())]))
            .color(col)
            .strong(),
        );
        let mic_rate = h.sidetone_mic_rate.load(r).max(1);
        let mic_block_frames = h.sidetone_mic_block_frames.load(r);
        let mic_period_ms = mic_block_frames as f32 * 1000.0 / mic_rate as f32;
        let out_period_ms = out_block_frames as f32 * 1000.0 / out_rate as f32;
        let underruns = h.sidetone_underruns.load(r);
        ui.horizontal(|ui| {
          ui.label(theme::caption(trf(
            "Microphone {} ms, output {} ms",
            &[&format!("{:.1}", mic_period_ms), &format!("{:.1}", out_period_ms)],
          )));
          ui.label(theme::caption("·"));
          ui.label(theme::caption(trf("Dropouts: {}", &[&underruns.to_string()])));
        });
      }
    }
  });

  ui.add_space(theme::CARD_GAP);

  // -------------------------------------------------------------------------------------
  // Card 2: Volume, Meter & Low Latency
  // -------------------------------------------------------------------------------------
  let peak = state
    .shm_header()
    .map_or(0.0, |h| (h.sidetone_peak_permille.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0));
  let low_lat = state.shm_header().map_or(false, |h| h.sidetone_low_latency.load(Ordering::Relaxed));
  let leveler_on = state.shm_header().is_none_or(|h| h.mic_leveler_enabled.load(Ordering::Relaxed));

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::VOLUME, ACCENT)), tr("Volume"));
    egui::Grid::new("sidetone_vol_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
      ui.label(tr("Volume"));
      // 0 % .. 300 %, the same scale as the microphone's "Sensitivity".
      ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = 280.0;
        if ui.add(egui::Slider::new(&mut settings.volume, 0.0..=VOLUME_MAX).show_value(false)).changed() {
          changed = true;
        }
        let percent = format!("{:.0}", settings.volume * 100.0);
        let readout = if settings.volume <= 1.0 {
          format!("{percent} %")
        } else {
          trf("{} % (+{} dB boost)", &[&percent, &format!("{:.1}", 20.0 * settings.volume.log10())])
        };
        ui.label(theme::strong(readout, 14.0).color(theme::adapt(ui, ACCENT)));
        if ui.button(tr("Reset to 100 %")).clicked() {
          settings.volume = 1.0;
          changed = true;
        }
      });
      ui.end_row();

      ui.label(tr("Level"));
      ui.horizontal(|ui| {
        let db_text = if peak > 0.0001 {
          format!("{:.0} % ({:.1} dBFS)", (peak * 100.0).round(), 20.0 * peak.log10())
        } else {
          "0 % (-inf dBFS)".to_string()
        };
        theme::meter(ui, peak, 280.0, theme::level_color(ui, peak), &db_text);
      });
      ui.end_row();

      ui.label(tr("Low latency"));
      ui.vertical(|ui| {
        if theme::toggle_labeled(ui, low_lat, "") {
          if let Some(h) = state.shm_header() {
            h.sidetone_low_latency.store(!low_lat, Ordering::Relaxed);
          }
        }
        ui.label(theme::caption(tr("Asks Windows for its shortest audio period on both devices. Uses more CPU.")));
      });
      ui.end_row();
    });
    ui.label(theme::caption(if leveler_on {
      tr("100 % = your voice as loud as the voice leveler makes it for the others, whatever the microphone's own level. The volume of the output device applies on top.")
    } else {
      tr("The voice leveler is off: 100 % = the microphone as it comes in. The volume of the output device applies on top.")
    }));
  });

  ui.add_space(theme::CARD_GAP);

  // -------------------------------------------------------------------------------------
  // Card 3: Noise suppression
  // -------------------------------------------------------------------------------------
  let (active_mode_u32, running_delay_us, gate_db) = state.shm_header().map_or((0, 0, 0.0), |h| {
    let r = Ordering::Relaxed;
    (
      h.sidetone_noise_mode_active.load(r),
      h.sidetone_noise_delay_us.load(r),
      h.sidetone_gate_gain_db_x10.load(r) as f32 / 10.0,
    )
  });
  let active_mode = NoiseMode::from_u32(active_mode_u32);

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::MIC, ACCENT)), tr("Noise suppression"));

    let format_mode_label = |mode: NoiseMode, delay_us: u32, is_active: bool| match mode {
      NoiseMode::Off => tr("Off").to_string(),
      NoiseMode::Expander => tr("Band expander (no delay)").to_string(),
      NoiseMode::WebRtcNs => {
        let ms_str = if is_active && delay_us > 0 {
          format!("{:.0}", (delay_us as f32 / 1000.0).round())
        } else {
          "7".to_string()
        };
        trf("WebRTC noise suppressor (+{} ms)", &[&ms_str])
      }
      NoiseMode::RnNoise => {
        let ms_str = if is_active && delay_us > 0 {
          format!("{:.0}", (delay_us as f32 / 1000.0).round())
        } else {
          "10".to_string()
        };
        trf("RNNoise (+{} ms)", &[&ms_str])
      }
      NoiseMode::RnNoise2 => {
        let ms_str = if is_active && delay_us > 0 {
          format!("{:.0}", (delay_us as f32 / 1000.0).round())
        } else {
          "20".to_string()
        };
        trf("RNNoise 0.2 (+{} ms)", &[&ms_str])
      }
    };

    ui.horizontal(|ui| {
      ui.label(tr("Noise suppression"));
      let current_label = format_mode_label(settings.noise_mode, running_delay_us, active_mode == settings.noise_mode);
      egui::ComboBox::from_id_salt("sidetone_noise_mode").selected_text(current_label).width(280.0).show_ui(ui, |ui| {
        for mode in [NoiseMode::Off, NoiseMode::Expander, NoiseMode::WebRtcNs, NoiseMode::RnNoise, NoiseMode::RnNoise2]
        {
          let label = format_mode_label(mode, running_delay_us, active_mode == mode);
          if ui.selectable_value(&mut settings.noise_mode, mode, label).changed() {
            changed = true;
          }
        }
      });
    });

    if active_mode_u32 != settings.noise_mode.to_u32() {
      if active_mode == NoiseMode::Expander && settings.noise_mode.is_framed() {
        theme::status_line(
          ui,
          Severity::Warning,
          tr("Running the band expander: this mode needs the microphone at 48 kHz."),
        );
      } else if settings.noise_mode == NoiseMode::RnNoise2 && active_mode == NoiseMode::RnNoise {
        theme::status_line(ui, Severity::Warning, tr("Running RNNoise: RNNoise 0.2 could not be loaded."));
      }
    }

    if settings.noise_mode == NoiseMode::Expander {
      ui.label(theme::caption(tr("The expander removes steady noise in pauses. Loud short sounds (keys) pass.")));
    }

    egui::Grid::new("sidetone_noise_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
      if settings.noise_mode == NoiseMode::Expander {
        ui.label(tr("Sensitivity"));
        ui.horizontal(|ui| {
          ui.spacing_mut().slider_width = 280.0;
          if ui
            .add(
              egui::Slider::new(&mut settings.gate_margin_db, GATE_MARGIN_DB_MIN..=GATE_MARGIN_DB_MAX)
                .step_by(1.0)
                .suffix(" dB"),
            )
            .changed()
          {
            changed = true;
          }
        });
        ui.end_row();
      }

      if settings.noise_mode != NoiseMode::Off && settings.noise_mode != NoiseMode::WebRtcNs {
        ui.label(tr("Depth"));
        ui.horizontal(|ui| {
          ui.spacing_mut().slider_width = 280.0;
          if ui
            .add(egui::Slider::new(&mut settings.gate_range_db, 0.0..=GATE_RANGE_DB_MAX).step_by(1.0).suffix(" dB"))
            .changed()
          {
            changed = true;
          }
        });
        ui.end_row();
      }

      if settings.noise_mode == NoiseMode::Expander {
        ui.label(tr("Reduction now"));
        ui.horizontal(|ui| {
          let reduction = (-gate_db).max(0.0);
          let max_range = settings.gate_range_db.max(1.0);
          let fraction = (reduction / max_range).clamp(0.0, 1.0);
          let val_text = format!("{:.1} dB", -gate_db);
          theme::meter(ui, fraction, 280.0, theme::palette(ui).accent, &val_text);
        });
        ui.end_row();
      }
    });
  });

  ui.add_space(theme::CARD_GAP);

  // -------------------------------------------------------------------------------------
  // Card 4: EQ, the same editor as the voice EQ on the Microphone page
  // -------------------------------------------------------------------------------------
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::EQUALIZER, EQ_COLOR)), tr("Sidetone EQ"));
    ui.label(theme::caption(tr(
      "Only for what you hear of yourself; others still get the voice EQ of the Microphone page. Changes are live and saved with the pair.",
    )));
    let height = super::eq_editor::plot_height(ui, 0.3, 180.0, 340.0);
    eq_changed = super::eq_editor::band_editor(ui, "sidetone_eq", &mut pair.bands, MAX_EQ_BANDS, EQ_COLOR, &[], height);
  });

  ui.add_space(theme::CARD_GAP);

  // -------------------------------------------------------------------------------------
  // Card 5: Saved pairs
  // -------------------------------------------------------------------------------------
  let (mut saved_pairs, _) = sidetone::pairs();
  let mut to_remove = None;
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::SETTINGS, ACCENT)), tr("Saved pairs"));
    for p in &saved_pairs.pairs {
      ui.horizontal(|ui| {
        ui.label(theme::strong(&trf("{} → {}", &[&p.mic_name, &p.output_name]), 14.0));
        let state_str = if p.enabled { tr("On") } else { tr("Off") };
        ui.label(RichText::new(state_str).color(theme::palette(ui).text_secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          if ui.button(tr("Delete")).clicked() {
            to_remove = Some((p.mic_id.clone(), p.output_id.clone()));
          }
        });
      });
    }
  });

  if let Some((rm_mic, rm_out)) = to_remove {
    saved_pairs.remove(&rm_mic, &rm_out);
    sidetone::save(saved_pairs);
    if pair.mic_id == rm_mic && pair.output_id == rm_out {
      state.sidetone_ui.pair = None;
    }
  }

  // -------------------------------------------------------------------------------------
  // Sync to shared memory & disk
  // -------------------------------------------------------------------------------------
  if changed || eq_changed {
    pair.set_settings(&settings);
    pair.bands.truncate(MAX_EQ_BANDS);
    state.sidetone_ui.dirty = true;
    if let Some(h) = state.shm_header() {
      let r = Ordering::Relaxed;
      // Before the settings: a pair switched on must not start with another pair's EQ.
      if eq_changed || enabled_toggled {
        h.write_sidetone_eq(&pair.bands);
      }
      h.write_sidetone(&settings);
      if enabled_toggled {
        if settings.enabled {
          h.sidetone_mic_hash.store(device_hash(&pair.mic_id), r);
          h.sidetone_output_hash.store(device_hash(&pair.output_id), r);
        } else {
          h.sidetone_mic_hash.store(0, r);
          h.sidetone_output_hash.store(0, r);
        }
      }
    }
    state.sidetone_ui.pair = Some(pair.clone());
  }

  // Persist when user releases the pointer
  if state.sidetone_ui.dirty && !ui.ctx().input(|i| i.pointer.any_down()) {
    let (mut saved_pairs, _) = sidetone::pairs();
    saved_pairs.upsert(pair);
    sidetone::save(saved_pairs);
    state.sidetone_ui.dirty = false;
  }
}
