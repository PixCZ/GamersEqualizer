use super::state::AppState;
use super::tab_mic::{ACCENT, EQ_COLOR};
use super::theme::{self, Severity, icons};
use crate::i18n::{tr, trf};
use crate::win_audio::apo_install::ApoStatus;
use crate::win_audio::sidetone::{self, BEAT_FRESH, Verdict};
use crate::win_audio::{AudioEndpointInfo, WinAudioSystem};
use egui::{RichText, Ui};
use gamersequalizer_common::biquad::MAX_EQ_BANDS;
use gamersequalizer_common::sidetone::{
  GATE_MARGIN_DB_MAX, GATE_MARGIN_DB_MIN, GATE_RANGE_DB_MAX, NoiseMode, SidetonePair, VOLUME_MAX, device_hash,
};
use gamersequalizer_mic_chain::sidetone_denoise::{SAMPLE_RATE, nominal_delay_ms};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// The default devices are a Windows audio call: asked twice a second, not every frame.
const DEVICES_RECHECK: Duration = Duration::from_millis(500);

pub fn render_tab_sidetone(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  theme::page_header(
    ui,
    icons::HEADPHONES,
    ACCENT,
    tr("Sidetone"),
    Some(tr("Sealed headphones block your own voice. Sidetone plays the microphone back into them.")),
  );

  // 1. The pair the worker follows (same rule: `sidetone::active_pair`), and its names.
  let (mut saved_pairs, _) = sidetone::pairs();
  if state.sidetone_ui.devices_checked.is_none_or(|t| t.elapsed() >= DEVICES_RECHECK) {
    state.sidetone_ui.devices_checked = Some(Instant::now());
    state.sidetone_ui.devices = sidetone::active_pair(
      state.selected_mic_endpoint_id.clone(),
      || win_audio.endpoint_lock.get_default_capture_id(),
      win_audio.endpoint_lock.get_default_output_id(),
    );
  }
  let Some(pair_key) = state.sidetone_ui.devices.clone() else {
    state.save_sidetone_pair();
    theme::status_line(ui, Severity::Warning, tr("Sidetone needs a microphone and an output device."));
    return;
  };
  let name_of = |list: &[AudioEndpointInfo], id: &str| {
    list.iter().find(|e| e.id == id).map_or_else(|| tr("Unknown device").to_string(), |e| e.name.clone())
  };
  let mic_name = name_of(state.capture_endpoints.as_slice(), &pair_key.0);
  let out_name = name_of(state.endpoints.as_slice(), &pair_key.1);

  // 2. Load the pair; edits of the previous one are saved first (the device switched mid-drag).
  if state.sidetone_ui.pair.is_none() || state.sidetone_ui.pair_key != pair_key {
    if let Some(saved) = state.save_sidetone_pair() {
      saved_pairs = saved;
    }
    let pair = saved_pairs
      .find(&pair_key.0, &pair_key.1)
      .cloned()
      .unwrap_or_else(|| SidetonePair::new(&pair_key.0, &pair_key.1, &mic_name, &out_name));
    state.sidetone_ui.pair = Some(pair);
    state.sidetone_ui.pair_key = pair_key.clone();
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
  let reader_running = state.sidetone_ui.last_reader_beat_time.is_some_and(|t| now.duration_since(t) < BEAT_FRESH);
  let writer_running = state.sidetone_ui.last_writer_beat_time.is_some_and(|t| now.duration_since(t) < BEAT_FRESH);

  if state.sidetone_ui.testing
    && let Some(h) = state.shm_header()
  {
    h.sidetone_test_beat.fetch_add(1, Ordering::Relaxed);
  }

  let mut pair = state.sidetone_ui.pair.clone().unwrap_or_default();
  pair.mic_name = mic_name;
  pair.output_name = out_name;
  let mut settings = pair.settings();
  let mut changed = false;
  let mut eq_changed = false;
  let verdict = state
    .shm_header()
    // What the daemon published, not only the switch here: right after switching on, its hashes
    // are not out yet and the verdict would blame another device.
    .map(|h| {
      let enabled = settings.enabled && h.read_sidetone().enabled;
      sidetone::verdict(h, enabled, writer_running, reader_running, state.sidetone_ui.testing)
    });

  // -------------------------------------------------------------------------------------
  // Card 1: Active pair, toggle, status & try-it
  // -------------------------------------------------------------------------------------

  theme::card(ui, |ui| {
    ui.horizontal(|ui| {
      theme::card_title(ui, Some((icons::HEADPHONES, ACCENT)), &trf("{} → {}", &[&pair.mic_name, &pair.output_name]));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if theme::toggle_labeled(ui, settings.enabled, tr("On for this pair")) {
          settings.enabled = !settings.enabled;
          changed = true;
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
    } else {
      let (severity, text) = match verdict.unwrap_or(Verdict::Starting) {
        Verdict::Off => (Severity::Info, tr("Off for this pair.").to_string()),
        Verdict::Muted => (Severity::Warning, tr("The microphone is muted: sidetone is silent.").to_string()),
        Verdict::OtherMicrophone => {
          (Severity::Info, trf("Another microphone is in use: sidetone waits for {}.", &[&pair.mic_name]))
        }
        Verdict::OtherOutput => {
          (Severity::Info, trf("Another output is playing: sidetone waits for {}.", &[&pair.output_name]))
        }
        Verdict::NoRecorder => (Severity::Info, tr("Waiting: no app is recording from this microphone.").to_string()),
        Verdict::Playing => (Severity::Success, tr("Playing.").to_string()),
        Verdict::Starting => (Severity::Info, tr("Starting…").to_string()),
      };
      theme::status_line(ui, severity, &text);
    }

    ui.add_space(8.0);

    // Try-it button
    ui.horizontal(|ui| {
      ui.add_enabled_ui(settings.enabled, |ui| {
        let btn_label = if state.sidetone_ui.testing { tr("Stop trying") } else { tr("Try it") };
        let btn =
          if state.sidetone_ui.testing { ui.button(btn_label) } else { ui.add(theme::accent_button(ui, btn_label)) };
        if btn.clicked() {
          state.sidetone_ui.testing = !state.sidetone_ui.testing;
        }
      });
      ui.label(theme::caption(tr("Plays without a recording app. Windows shows the microphone as in use meanwhile.")));
    });

    // Latency report when reader is active
    if reader_running && let Some(h) = state.shm_header() {
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
        theme::strong(trf("Delay about {} ms (without the converters)", &[&format!("{:.0}", delay_ms.round())]), 14.0)
          .color(col),
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
        ui.separator();
        ui.label(theme::caption(trf("Dropouts: {}", &[&underruns.to_string()])));
      });
    }
  });

  ui.add_space(theme::CARD_GAP);

  // -------------------------------------------------------------------------------------
  // Card 2: Volume, Meter & Low Latency
  // -------------------------------------------------------------------------------------
  let peak = state
    .shm_header()
    .map_or(0.0, |h| (h.sidetone_peak_permille.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0));
  let low_lat = state.shm_header().is_some_and(|h| h.sidetone_low_latency.load(Ordering::Relaxed));
  let leveler_on = state.shm_header().is_none_or(|h| h.mic_leveler_enabled.load(Ordering::Relaxed));

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::VOLUME, ACCENT)), tr("Volume"));
    egui::Grid::new("sidetone_vol_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
      ui.label(tr("Volume"));
      // 0 % .. 300 %, the same scale as the microphone's "Sensitivity".
      changed |= theme::percent_volume(ui, &mut settings.volume, VOLUME_MAX, 280.0, ACCENT);
      ui.end_row();

      ui.label(tr("Level"));
      ui.horizontal(|ui| {
        theme::meter(ui, peak, 280.0, theme::level_color(ui, peak), &theme::level_text(peak));
      });
      ui.end_row();

      ui.label(tr("Low latency"));
      ui.vertical(|ui| {
        if theme::toggle_labeled(ui, low_lat, "")
          && let Some(h) = state.shm_header()
        {
          h.sidetone_low_latency.store(!low_lat, Ordering::Relaxed);
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
  let (active_mode_u32, running_delay_us, gate_db, mic_rate) = state.shm_header().map_or((0, 0, 0.0, 0), |h| {
    let r = Ordering::Relaxed;
    (
      h.sidetone_noise_mode_active.load(r),
      h.sidetone_noise_delay_us.load(r),
      h.sidetone_gate_gain_db_x10.load(r) as f32 / 10.0,
      h.sidetone_mic_rate.load(r),
    )
  });
  let active_mode = NoiseMode::from_u32(active_mode_u32);

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::MIC, ACCENT)), tr("Noise suppression"));

    // The measured delay while the mode runs, else what it adds on whole frames.
    let format_mode_label = |mode: NoiseMode, delay_us: u32, is_active: bool| {
      let ms = if is_active && delay_us > 0 { delay_us as f32 / 1000.0 } else { nominal_delay_ms(mode) };
      let ms = format!("{:.0}", ms.round());
      match mode {
        NoiseMode::Off => tr("Off").to_string(),
        NoiseMode::Expander => tr("Quiet pauses (no delay)").to_string(),
        NoiseMode::WebRtcNs => trf("WebRTC noise suppressor (+{} ms)", &[&ms]),
        NoiseMode::RnNoise => trf("RNNoise (+{} ms)", &[&ms]),
        NoiseMode::RnNoise2 => trf("RNNoise 0.2 (+{} ms)", &[&ms]),
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
        // At 48 kHz the mode is being built (a moment after it is picked), or could not be.
        let why = if mic_rate == SAMPLE_RATE {
          tr("Running quiet pauses instead: this mode has not started. If this stays, restart the recording app.")
        } else {
          tr("Running quiet pauses instead: this mode needs the microphone at 48 kHz.")
        };
        theme::status_line(ui, Severity::Warning, why);
      } else if settings.noise_mode == NoiseMode::RnNoise2 && active_mode == NoiseMode::RnNoise {
        theme::status_line(ui, Severity::Warning, tr("Running RNNoise: RNNoise 0.2 could not be loaded."));
      }
    }

    if settings.noise_mode == NoiseMode::Expander {
      ui.label(theme::caption(tr(
        "Turns down steady noise in the pauses between words. Loud short sounds (keys) pass.",
      )));
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
  let mut to_remove = None;
  // Deleting takes the pair's whole setup and EQ with no undo: one click only asks.
  let mut confirm = state.sidetone_ui.confirm_delete.take();
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::SETTINGS, ACCENT)), tr("Saved pairs"));
    for p in &saved_pairs.pairs {
      let key = (p.mic_id.clone(), p.output_id.clone());
      let asking = confirm.as_ref() == Some(&key);
      ui.horizontal(|ui| {
        ui.label(theme::strong(trf("{} → {}", &[&p.mic_name, &p.output_name]), 14.0));
        let state_str = if p.enabled { tr("On") } else { tr("Off") };
        ui.label(RichText::new(state_str).color(theme::palette(ui).text_secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          if !asking && ui.button(tr("Delete…")).clicked() {
            confirm = Some(key.clone());
          }
        });
      });
      if asking {
        theme::status_line(
          ui,
          Severity::Warning,
          tr("Delete this pair? Its whole sidetone setup goes with it, the EQ included, and cannot be restored."),
        );
        ui.horizontal(|ui| {
          let err = theme::palette(ui).err;
          if ui.button(RichText::new(theme::with_icon(icons::DELETE, tr("Delete"))).color(err)).clicked() {
            to_remove = Some(key.clone());
            confirm = None;
          }
          if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            confirm = None;
          }
        });
      }
    }
  });
  state.sidetone_ui.confirm_delete = confirm;

  if let Some((rm_mic, rm_out)) = to_remove {
    saved_pairs.remove(&rm_mic, &rm_out);
    sidetone::save(saved_pairs.clone());
    if pair.mic_id == rm_mic && pair.output_id == rm_out {
      state.sidetone_ui.pair = None;
      state.sidetone_ui.dirty = false;
      return;
    }
  }

  // -------------------------------------------------------------------------------------
  // Live values to shared memory, the pair to disk
  // -------------------------------------------------------------------------------------
  if changed || eq_changed {
    pair.set_settings(&settings);
    pair.bands.truncate(MAX_EQ_BANDS);
    state.sidetone_ui.dirty = true;
    // Live while dragging, only into the pair the worker has published (its hashes are 0 while
    // it is off). Switching on or off, and everything else, goes through the saved file: the
    // worker is the only one that publishes a pair and its hashes (within ~1 s of the save).
    if let Some(h) = state.shm_header() {
      let r = Ordering::Relaxed;
      let published = h.sidetone_mic_hash.load(r) == device_hash(&pair.mic_id)
        && h.sidetone_output_hash.load(r) == device_hash(&pair.output_id);
      if published {
        if eq_changed {
          h.write_sidetone_eq(&pair.bands);
        }
        h.write_sidetone_values(&settings);
      }
    }
    state.sidetone_ui.pair = Some(pair);
  }
  // Saved by `AppState::save_pending` once the pointer is released.
}
