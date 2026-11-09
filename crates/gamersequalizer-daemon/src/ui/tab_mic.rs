use super::state::{AecTelemetry, AppState, DenoiseStatus};
use super::theme::{self, icons, Severity};
use crate::i18n::{tr, trf};
use crate::win_audio::WinAudioSystem;
use egui::{Color32, RichText, Stroke, Ui};
use egui_plot::{HLine, Legend, Line, Plot, PlotPoints, VLine};
use gamersequalizer_common::biquad::MAX_EQ_BANDS;
use gamersequalizer_common::shm::{
  AecState, DenoiseLimit, DenoiseModel, NOISE_GATE_AUTO, NOISE_GATE_AUTO_LOOKAHEAD, NOISE_GATE_OFF,
};
use gamersequalizer_mic_vst::ai_denoiser::AiVoiceIsolator;
use gamersequalizer_mic_vst::delay_estimator::{FULL_SCALE_PEAK_RATIO, MIN_PEAK_RATIO};
use gamersequalizer_mic_vst::voice_leveler::{DEFAULT_TARGET_DB, MAX_TARGET_DB, MIN_TARGET_DB};
use std::sync::atomic::Ordering;

const ACCENT: Color32 = Color32::from_rgb(247, 127, 0);
const AEC_COLOR: Color32 = Color32::from_rgb(0, 200, 255);
const AI_COLOR: Color32 = Color32::from_rgb(140, 240, 120);
const EQ_COLOR: Color32 = Color32::from_rgb(255, 180, 80);
const OK_COLOR: Color32 = Color32::from_rgb(80, 220, 120);
/// Lowest level shown on the dBFS meters.
const METER_FLOOR_DB: f32 = -80.0;
/// Width of the meters and bars in the AEC grid.
const BAR_WIDTH: f32 = 280.0;
/// Extra horizontal gap between the three AEC columns.
const AEC_COLUMN_GAP: f32 = 12.0;
/// AEC grid: label column width, room for the value after a meter, row height (= meter).
const AEC_LABEL_WIDTH: f32 = 110.0;
const AEC_VALUE_WIDTH: f32 = 70.0;
const AEC_ROW_HEIGHT: f32 = 20.0;
const AEC_PLOT_HEIGHT: f32 = 120.0;
/// Fixed y-axis label width ("0.05"), so the plot area doesn't resize when labels change.
const AEC_Y_AXIS_WIDTH: f32 = 30.0;
/// Clipping this recent is shown as a warning.
const CLIP_RECENT_SECS: f64 = 60.0;

pub fn render_tab_mic(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  egui::ScrollArea::vertical().id_salt("mic_page").auto_shrink([false, false]).show(ui, |ui| {
    theme::page_header(
      ui,
      icons::MIC,
      ACCENT,
      tr("Microphone"),
      Some(tr("Chain (Windows effect on the microphone, so every app gets it): echo cancellation → AI noise suppression → voice EQ → voice leveler → boost/limiter.")),
    );
    super::apo_banner::render_apo_status_line(ui, state);
    latency_line(ui, state);
    ui.add_space(theme::CARD_GAP);

    input_section(ui, state, win_audio);
    aec_section(ui, state);
    denoise_section(ui, state);
    eq_section(ui, state);
    leveler_section(ui, state);
  });
}

/// The user picks a mic (`Some`) or goes back to the Windows default (`None`). Only a pick is
/// saved; without one, the app and the service follow the Windows default microphone.
pub(super) fn pick_mic(state: &mut AppState, win_audio: &mut WinAudioSystem, pick: Option<String>) {
  state.selected_mic_endpoint_id = pick.clone();
  state.set_shm_selected_mic(pick.as_deref().unwrap_or_default());
  win_audio.chatmix.use_mic(pick);
  win_audio.mic_stream.set_input_device(win_audio.chatmix.mic_device_id.clone());
}

fn leveler_section(ui: &mut Ui, state: &mut AppState) {
  let Some(h) = state.shm_header() else {
    return;
  };
  let on = h.mic_leveler_enabled.load(Ordering::Relaxed);
  let gain_db = h.mic_leveler_gain_db_x10.load(Ordering::Relaxed) as f32 / 10.0;
  theme::card(ui, |ui| {
    ui.horizontal(|ui| {
      theme::card_title(ui, Some((icons::MIC, ACCENT)), tr("Stage 4: Voice leveler"));
      ui.label(theme::icon(icons::INFO, 14.0).color(theme::palette(ui).text_secondary)).on_hover_text(tr(
        "Brings your voice to the same loudness whether you talk softly or loudly, or the mic is set low. \
         It learns your level only while it hears you speak: quickly in the first seconds, then ever slower, \
         so one shout or cough changes nothing. Between words the gain stays put, it does not pump up the noise.",
      ));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let mut new_on = on;
        if ui.add(theme::Toggle(&mut new_on)).on_hover_text(tr("Enable voice leveler")).changed() {
          h.mic_leveler_enabled.store(new_on, Ordering::Relaxed);
        }
      });
    });
    if !on {
      return;
    }
    egui::Grid::new("leveler_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
      ui.label(tr("Target loudness:"));
      ui.horizontal(|ui| {
        let mut target = h.mic_leveler_target_db_x10.load(Ordering::Relaxed) as f32 / 10.0;
        ui.spacing_mut().slider_width = BAR_WIDTH;
        let slider = egui::Slider::new(&mut target, MIN_TARGET_DB..=MAX_TARGET_DB).step_by(1.0).show_value(false);
        if ui.add(slider).on_hover_text(tr("How loud others hear you. The leveler gets your voice there by itself.")).changed() {
          h.mic_leveler_target_db_x10.store((target * 10.0).round() as i32, Ordering::Relaxed);
        }
        let word = if target <= DEFAULT_TARGET_DB - 4.0 {
          tr("quieter")
        } else if target >= DEFAULT_TARGET_DB + 4.0 {
          tr("louder")
        } else {
          tr("normal")
        };
        ui.label(format!("{word} ({target:.0} dBFS)"));
        if (target - DEFAULT_TARGET_DB).abs() > 0.5 && ui.small_button(tr("Default")).clicked() {
          h.mic_leveler_target_db_x10.store((DEFAULT_TARGET_DB * 10.0).round() as i32, Ordering::Relaxed);
        }
      });
      ui.end_row();

      ui.label(tr("Now:"));
      ui.horizontal(|ui| {
        let text = if gain_db.abs() < 0.05 {
          tr("Listening: say a few words").to_string()
        } else {
          trf("Gain {} dB", &[&format!("{gain_db:+.1}")])
        };
        ui.label(text);
        if ui
          .button(tr("Learn again"))
          .on_hover_text(tr("Forget the learned level of this microphone and learn it from your next words (new mic, mic moved)."))
          .clicked()
        {
          h.mic_leveler_reset_seq.fetch_add(1, Ordering::Relaxed);
        }
      });
      ui.end_row();
    });
  });
}

fn input_section(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  theme::card(ui, |ui| {
    ui.horizontal(|ui| {
      theme::card_title(ui, Some((icons::MIC, ACCENT)), tr("Microphone input & sensitivity"));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let p = theme::palette(ui);
        let (mute_text, mute_color) = if win_audio.chatmix.mic_muted {
          (theme::with_icon(icons::MUTE, tr("Muted")), p.err)
        } else {
          (theme::with_icon(icons::MIC, tr("Active")), p.text)
        };
        if ui.button(RichText::new(mute_text).color(mute_color)).clicked() {
          win_audio.chatmix.mic_muted = !win_audio.chatmix.mic_muted;
        }
      });
    });

    egui::Grid::new("mic_input_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
      // Hardware microphone selector
      ui.label(tr("Input device"));
      ui.horizontal(|ui| {
        let current_id = state.selected_mic_endpoint_id.clone();
        let current_name = current_id
          .as_ref()
          .and_then(|mid| state.capture_endpoints.iter().find(|e| &e.id == mid))
          .map(|e| e.name.clone())
          .unwrap_or_else(|| tr("Default microphone").to_string());

        let mut changed_mic = None;
        egui::ComboBox::from_id_salt("combo_tab_mic_device").selected_text(&current_name).width(320.0).show_ui(
          ui,
          |ui| {
            if ui.selectable_label(current_id.is_none(), tr("Default microphone")).clicked() {
              changed_mic = Some(None);
            }
            for ep in &state.capture_endpoints {
              let is_sel = current_id.as_deref() == Some(&ep.id);
              if ui.selectable_label(is_sel, &ep.name).clicked() {
                changed_mic = Some(Some(ep.id.clone()));
              }
            }
          },
        );

        if let Some(pick) = changed_mic {
          pick_mic(state, win_audio, pick);
        }

        if let Some(mid) = state.selected_mic_endpoint_id.clone() {
          if super::apo_banner::selected_mic_is_default(state) {
            theme::status_line(ui, Severity::Success, tr("Windows default"));
          } else if ui
            .add(theme::accent_button(ui, tr("Set as Windows default mic")))
            .on_hover_text(tr("Discord, games and other apps use the Windows default microphone."))
            .clicked()
          {
            super::apo_banner::set_default_mic(state, win_audio, &mid);
          }
        }
      });
      ui.end_row();

      // Live input meter
      ui.label(tr("Live input level"));
      let peak = win_audio.chatmix.mic_peak_level;
      let db_text = if peak > 0.0001 {
        format!("{:.0} % ({:.1} dBFS)", (peak * 100.0).round(), 20.0 * peak.log10())
      } else {
        "0 % (-inf dBFS)".to_string()
      };
      theme::meter(ui, peak, BAR_WIDTH * 1.6, theme::level_color(ui, peak), &db_text);
      ui.end_row();

      if let Some(clips) = state.shm_header().map(|h| h.mic_input_clips.load(Ordering::Relaxed)) {
        ui.label(tr("Clipping"));
        clipping_status(ui, clips);
        ui.end_row();
      }

      // Sensitivity 0% .. 300%
      ui.label(tr("Sensitivity"));
      ui.horizontal(|ui| {
        let mut vol = win_audio.chatmix.mic_volume;
        ui.spacing_mut().slider_width = BAR_WIDTH;
        if ui.add(egui::Slider::new(&mut vol, 0.0..=3.0).show_value(false)).changed() {
          win_audio.chatmix.mic_volume = vol;
        }
        let readout = if vol <= 1.0 {
          trf("{} % (native)", &[&format!("{:.0}", vol * 100.0)])
        } else {
          trf("{} % (+{} dB boost)", &[&format!("{:.0}", vol * 100.0), &format!("{:.1}", 20.0 * vol.log10())])
        };
        ui.label(theme::strong(readout, 14.0).color(theme::adapt(ui, EQ_COLOR)));
        if ui.button(tr("Reset to 100 %")).clicked() {
          win_audio.chatmix.mic_volume = 1.0;
        }
      });
      ui.end_row();
    });

    let r = Ordering::Relaxed;
    let leveler = state.shm_header().map(|h| (h.mic_leveler_enabled.load(r), h.mic_auto_gain_permille.load(r)));
    let caption = match leveler {
      Some((true, _)) => tr(
        "With the voice leveler on this is how loud others hear you: 100 % = the leveler's target, 50 % = 6 dB under it. The Windows input level only keeps the signal clean; the app moves it by itself when the mic clips or is far too quiet.",
      ),
      _ => tr(
        "0 % - 100 % moves the Windows input slider. 100 % - 300 % adds a soft-saturated digital boost up to +9.5 dB (3x amplitude).",
      ),
    };
    ui.label(theme::caption(caption));
    if let Some((true, auto)) = leveler.filter(|&(_, auto)| auto > 0) {
      ui.label(theme::caption(trf("Windows input level set automatically: {} %", &[&format!("{:.0}", auto as f32 / 10.0)])));
    }
  });
}

/// Shows whether the raw mic clipped recently (DSP counter: full-scale samples and flat tops).
/// The counter is cumulative; the time of the last increase lives in egui's memory.
fn clipping_status(ui: &mut Ui, clips: u32) {
  let id = egui::Id::unique("mic_clip_seen");
  let now = ui.input(|i| i.time);
  let (seen, last_at) = ui.ctx().data(|d| d.get_temp::<(u32, Option<f64>)>(id)).unwrap_or((clips, None));
  // A smaller counter = the DSP restarted (new stream): no new clipping.
  let last_at = if clips > seen { Some(now) } else { last_at };
  ui.ctx().data_mut(|d| d.insert_temp(id, (clips, last_at)));
  match last_at.map(|t| now - t).filter(|&ago| ago < CLIP_RECENT_SECS) {
    Some(ago) => {
      theme::status_line(ui, Severity::Warning, &trf("Mic clipped {} s ago", &[&format!("{:.0}", ago)]))
        .on_hover_text(tr(
          "The sound card's input distorts: the wave is cut at its top. Lower the Windows input level, and check \
           \"Microphone Boost\" in the Windows sound control panel (device properties, Levels): it amplifies before \
           the input level, so lowering the level alone doesn't help. With the voice leveler on the app lowers the \
           input level by itself.",
        ));
    }
    None => {
      theme::status_line(ui, Severity::Success, tr("No clipping"));
    }
  }
}

fn aec_section(ui: &mut Ui, state: &mut AppState) {
  theme::card(ui, |ui| {
    // Header: title, description in a tooltip, on/off switch on the right.
    let (aec_enabled, ai_strength, gate_db) = mic_controls(state);
    ui.horizontal(|ui| {
      theme::card_title(ui, Some((icons::SPEAKERS, AEC_COLOR)), tr("Stage 1: Speaker echo cancellation (AEC)"));
      ui.label(theme::icon(icons::INFO, 14.0).color(theme::palette(ui).text_secondary)).on_hover_text(tr(
        "Speaker output is captured (WASAPI loopback) and removed from the mic by WebRTC's echo canceller (AEC3): an adaptive filter plus suppression of the echo it leaves. The echo delay is measured by cross-correlation (GCC-PHAT), remembered per device pair and only changed after repeated confirmation.",
      ));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let mut on = aec_enabled;
        if ui.add(theme::Toggle(&mut on)).on_hover_text(tr("Enable speaker echo cancellation")).changed() {
          state.update_shm_mic_controls(on, ai_strength, gate_db);
        }
      });
    });

    let t = state.poll_aec_telemetry();
    let (status, severity) = aec_status_text(&t);
    theme::status_line(ui, severity, &status);

    if !aec_enabled {
      return;
    }

    // Columns: title, a grid (label | value). Same rows, row height, label width and text
    // styles everywhere, so they line up.
    ui.add_space(8.0);
    let p = theme::palette(ui);
    let gap = AEC_COLUMN_GAP + ui.spacing().item_spacing.x;
    let col_w = ((ui.available_width() - 2.0 * gap) / 3.0).max(200.0);
    let meter_w = (col_w - AEC_LABEL_WIDTH - AEC_VALUE_WIDTH).max(40.0);
    ui.horizontal_top(|ui| {
      aec_column(ui, col_w, tr("Echo"), |ui| {
        aec_row(ui, tr("Removed"), |ui| {
          aec_meter(ui, t.erle_db / 40.0, meter_w, p.accent, format!("{:.1} dB", t.erle_db));
        });
        let (delay, hover) = match t.delay_ms {
          Some(d) => {
            let src = if t.from_cache { tr("  (from previous session, re-checking)") } else { "" };
            (format!("{d:.2} ms"), trf("{} samples @ 48 kHz{}", &[&(d * 48.0).round(), &src]))
          }
          None => ("—".to_string(), tr("not measured yet: play some sound through the speakers").to_string()),
        };
        aec_row(ui, tr("Delay"), |ui| {
          ui.label(RichText::new(delay).color(p.ok)).on_hover_text(hover);
        });
      });
      ui.add_space(AEC_COLUMN_GAP);
      aec_column(ui, col_w, tr("Levels (dBFS)"), |ui| {
        // Not live: the levels are stale, show dashes instead.
        level_row(ui, tr("Speakers"), t.live.then_some(t.ref_db), meter_w);
        level_row(ui, tr("Mic in"), t.live.then_some(t.mic_db), meter_w);
        level_row(ui, tr("Mic out"), t.live.then_some(t.out_db), meter_w);
      });
    });

    // Troubleshooting only: how sure the delay measurement is, and its history.
    ui.add_space(4.0);
    egui::CollapsingHeader::new(tr("Details")).id_salt("aec_details").default_open(false).show(ui, |ui| {
      ui.horizontal_top(|ui| {
        aec_column(ui, col_w, tr("Delay measurement"), |ui| {
          aec_row(ui, tr("Confidence"), |ui| {
            aec_meter(ui, t.confidence, meter_w, p.accent, format!("{:.0} %", t.confidence * 100.0));
          });
          let last = t.raw_delay_ms.map_or_else(|| tr("no clear peak").to_string(), |d| format!("{d:.2} ms"));
          aec_row(ui, tr("Last measurement"), |ui| {
            ui.label(RichText::new(last).color(p.text)).on_hover_text(trf("{} measurements total", &[&t.estimate_count]));
          });
        });
      });
      ui.add_space(8.0);
      ui.horizontal_top(|ui| {
        aec_plot(ui, col_w, tr("Correlation vs. delay"), None, |ui| corr_plot(ui, &t, col_w));
        ui.add_space(AEC_COLUMN_GAP);
        aec_plot(ui, col_w, tr("History (60 s): ERLE dB / delay ms"), None, |ui| history_plot(ui, state, col_w));
      });
    });
  });
}

/// One AEC value block: bold title, then the rows in a grid (fixed label width and row
/// height, so all blocks match).
fn aec_column(ui: &mut Ui, width: f32, title: &str, rows: impl FnOnce(&mut Ui)) {
  ui.vertical(|ui| {
    ui.set_width(width);
    ui.label(theme::strong(title, 14.0));
    egui::Grid::new(("aec_grid", title))
      .num_columns(2)
      .min_col_width(AEC_LABEL_WIDTH)
      .min_row_height(AEC_ROW_HEIGHT)
      .spacing([8.0, 4.0])
      .show(ui, rows);
  });
}

/// One AEC plot: a one-line caption (truncated if too long, full text on hover), then the plot.
fn aec_plot(ui: &mut Ui, width: f32, caption: &str, hover: Option<&str>, plot: impl FnOnce(&mut Ui)) {
  ui.vertical(|ui| {
    ui.set_width(width);
    let text = RichText::new(caption).color(theme::palette(ui).text_secondary);
    let r = ui.add(egui::Label::new(text).truncate());
    if let Some(h) = hover {
      r.on_hover_text(h);
    }
    plot(ui);
  });
}

/// One grid row: label in the secondary color, then the value widget.
fn aec_row(ui: &mut Ui, label: &str, value: impl FnOnce(&mut Ui)) {
  ui.label(RichText::new(label).color(theme::palette(ui).text_secondary));
  value(ui);
  ui.end_row();
}

/// Meter with its value in the normal text style of the AEC rows.
fn aec_meter(ui: &mut Ui, fraction: f32, width: f32, fill: Color32, value: String) {
  let text = RichText::new(value).color(theme::palette(ui).text);
  theme::meter_with(ui, fraction, width, fill, text);
}

fn aec_status_text(t: &AecTelemetry) -> (String, Severity) {
  // Shared memory exists but no stream writes it: whatever state it holds is stale.
  if t.state.is_some() && !t.live {
    return (tr("Idle: no app is using the microphone now").into(), Severity::Info);
  }
  match t.state {
    Some(AecState::Disabled) => (tr("Disabled").into(), Severity::Info),
    Some(AecState::NoReference) => {
      (tr("No speaker reference (loopback not running or no timestamps)").into(), Severity::Error)
    }
    Some(AecState::ReferenceSilent) if t.delay_ms.is_some() => {
      (tr("Ready: nothing playing right now").into(), Severity::Success)
    }
    Some(AecState::ReferenceSilent) => (tr("Waiting for speaker audio to measure the delay").into(), Severity::Warning),
    Some(AecState::Measuring) => (tr("Measuring echo delay...").into(), Severity::Warning),
    Some(AecState::Locked) => (tr("Active: cancelling speaker echo").into(), Severity::Success),
    Some(AecState::Unknown(v)) => (trf("Unknown state {} (newer DSP build?)", &[&v]), Severity::Info),
    None => (tr("Mic DSP not running").into(), Severity::Info),
  }
}

/// dBFS level on a -80..0 dB scale (the column title says dBFS, so the value has no unit);
/// None = no data (a dash and an empty meter).
fn level_row(ui: &mut Ui, label: &str, db: Option<f32>, width: f32) {
  let (frac, text) = match db {
    None => (0.0, "—".to_string()),
    Some(db) if db <= METER_FLOOR_DB => (0.0, tr("silent").to_string()),
    Some(db) => (((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0), format!("{db:.1}")),
  };
  aec_row(ui, label, |ui| aec_meter(ui, frac, width, theme::level_color(ui, frac), text));
}

/// Y grid for a plot that starts at 0: lines at tenths and halves, labels at 0, half and `top`
/// (egui's default 1-2-5 steps leave only "0" labelled on a short plot with a range like 0.05).
fn y_steps(top: f64) -> impl Fn(egui_plot::GridInput) -> Vec<egui_plot::GridMark> {
  egui_plot::uniform_grid_spacer(move |_| [top / 10.0, top / 2.0, top])
}

fn small_plot(id: &str, width: f32) -> Plot<'_> {
  Plot::new(id)
    .width(width)
    .height(AEC_PLOT_HEIGHT)
    .allow_drag(false)
    .allow_zoom(false)
    .allow_scroll(false)
    .show_axes([true, true])
    .y_axis_min_width(AEC_Y_AXIS_WIDTH)
}

fn corr_plot(ui: &mut Ui, t: &AecTelemetry, width: f32) {
  let n = t.corr.len().max(1);
  let max_ms = if t.corr_max_delay_ms > 0.0 { t.corr_max_delay_ms } else { 400.0 };
  let pts: PlotPoints =
    t.corr.iter().enumerate().map(|(i, &v)| [((i as f32 + 0.5) / n as f32 * max_ms) as f64, v as f64]).collect();
  let (line, mark) = (theme::adapt(ui, AEC_COLOR), theme::adapt(ui, OK_COLOR));
  let threshold_color = theme::palette(ui).text_tertiary;
  small_plot("aec_corr_plot", width)
    .include_y(0.0)
    .include_y(1.0)
    .y_grid_spacer(y_steps(1.0))
    .include_x(0.0)
    .include_x(max_ms as f64)
    .x_axis_formatter(|v, _| format!("{:.0} ms", v.value))
    // The curve is peak-to-average ratio / full scale: label it as the ratio.
    .y_axis_formatter(|v, _| format!("{:.0}", v.value * FULL_SCALE_PEAK_RATIO as f64))
    .label_formatter(|pos| {
      let p = super::eq_editor::hover_point(pos);
      Some(format!("{:.1} ms\n{:.1}x", p.x, p.y * FULL_SCALE_PEAK_RATIO as f64))
    })
    .show(ui, |pu| {
      let threshold = (MIN_PEAK_RATIO / FULL_SCALE_PEAK_RATIO) as f64;
      pu.hline(HLine::new(tr("acceptance threshold"), threshold).stroke(Stroke::new(1.0_f32, threshold_color)));
      pu.line(Line::new("|corr|", pts).stroke(Stroke::new(1.5_f32, line)));
      if let Some(d) = t.delay_ms {
        pu.vline(VLine::new(tr("locked delay"), d as f64).stroke(Stroke::new(1.2_f32, mark)));
      }
    });
}

fn history_plot(ui: &mut Ui, state: &AppState, width: f32) {
  let now = state.aec_history.back().map_or(0.0, |p| p.t);
  let erle: PlotPoints = state.aec_history.iter().map(|p| [p.t - now, p.erle_db as f64]).collect();
  let delay: PlotPoints = state.aec_history.iter().filter_map(|p| p.delay_ms.map(|d| [p.t - now, d as f64])).collect();
  let (a, b) = (theme::adapt(ui, AEC_COLOR), theme::adapt(ui, OK_COLOR));
  small_plot("aec_history_plot", width)
    .include_x(-60.0)
    .include_x(0.0)
    .include_y(0.0)
    .include_y(40.0)
    .x_axis_formatter(|v, _| format!("{:.0} s", v.value))
    .legend(Legend::default())
    .show(ui, |pu| {
      pu.line(Line::new("ERLE dB", erle).stroke(Stroke::new(1.5_f32, a)));
      pu.line(Line::new(tr("delay ms"), delay).stroke(Stroke::new(1.5_f32, b)));
    });
}

/// At or below this strength the denoiser is bypassed (matches AiVoiceIsolator).
const DENOISE_OFF_STRENGTH: f32 = 0.001;
/// Strength restored when a model is picked again after "Off". Not 100 % (`strength_report`,
/// private voice, 2026-11-08): DFN3 and DPDFNet gain only ~1 dB in pauses (the gate covers them),
/// DFN3 loses more voice under clicks (5.8 -> 7.1 %), and RNNoise (stand-in in pauses, the click
/// rescue's source) loses 1.6 -> 12 % of soft syllables under loud keys.
const DENOISE_DEFAULT_STRENGTH: f32 = 0.75;

fn denoise_section(ui: &mut Ui, state: &mut AppState) {
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::EQUALIZER, AI_COLOR)), tr("Stage 2: AI noise suppression"));

    let (selected, _) = state.denoise_models();
    let (aec_enabled, ai_strength, gate_db) = mic_controls(state);
    // Off = strength 0: the denoiser and the noise gate are bypassed (see AiVoiceIsolator).
    let off = ai_strength <= DENOISE_OFF_STRENGTH;
    let ahead = gate_db >= NOISE_GATE_AUTO_LOOKAHEAD;
    ui.horizontal(|ui| {
      ui.label(tr("Model:"));
      // None = Off.
      let current = (!off).then_some(selected);
      let mut choice = current;
      ui.selectable_value(&mut choice, None, tr("Off")).on_hover_text(tr("No noise suppression and no noise gate."));
      for (model, about) in [
        (DenoiseModel::RnNoise, tr(RNNOISE_ABOUT)),
        (DenoiseModel::DeepFilterNet, tr(DEEPFILTER_ABOUT)),
        (DenoiseModel::DpdfNet, tr(DPDFNET_ABOUT)),
      ] {
        let label = format!("{} · {}", model_name(model), ms_text(total_latency_ms(model, ahead)));
        let clicked = ui
          .selectable_value(&mut choice, Some(model), label)
          .on_hover_text(format!("{about}\n\n{}", latency_breakdown(model, ahead)))
          .clicked();
        // The running pick clicked again: retry it (a failed load, or too slow before).
        if clicked && current == Some(model) {
          state.retry_denoise_model();
        }
      }
      if choice != current {
        match choice {
          None => state.update_shm_mic_controls(aec_enabled, 0.0, gate_db),
          Some(model) => {
            if model != selected {
              state.set_denoise_model(model);
            }
            if off {
              state.update_shm_mic_controls(aec_enabled, DENOISE_DEFAULT_STRENGTH, gate_db);
            }
          }
        }
      }
    });
    if off {
      theme::status_line(ui, Severity::Warning, tr("Noise suppression is off."));
      return;
    }
    let status = state.denoise_status();
    // Running as selected: nothing to say (the measured delay is at the top of the page).
    let note = match status {
      DenoiseStatus::Running(_) => None,
      DenoiseStatus::Idle(m) => Some((
        trf("No app is using the microphone now. {} starts with the next call/recording.", &[&model_name(m)]),
        Severity::Info,
      )),
      DenoiseStatus::Switching(m) => Some((trf("Switching... running: {}", &[&model_name(m)]), Severity::Warning)),
      DenoiseStatus::Limited { limit, running } => Some((
        match limit {
          DenoiseLimit::Overloaded => trf(
            "Limited: {} was too slow for real time on this CPU, {} runs instead. Pick {} again to retry.",
            &[&model_name(selected), &model_name(running), &model_name(selected)],
          ),
          DenoiseLimit::LoadFailed => trf(
            "{} could not be loaded by the audio engine, {} runs instead. Pick {} again to retry.",
            &[&model_name(selected), &model_name(running), &model_name(selected)],
          ),
          other => trf("Limited ({}), running: {}", &[&format!("{other:?}"), &model_name(running)]),
        },
        Severity::Error,
      )),
      DenoiseStatus::Failed { selected, running } => Some((
        trf("{} could not be loaded by the audio engine, still on {}.", &[&model_name(selected), &model_name(running)]),
        Severity::Error,
      )),
    };
    if let Some((note, severity)) = note {
      theme::status_line(ui, severity, &note);
    }
    if !matches!(status, DenoiseStatus::Idle(_)) {
      dsp_load_row(ui, state.mic_dsp_load());
    }

    let (aec_enabled, mut ai_strength, gate_db) = mic_controls(state);
    let mut changed = false;
    egui::Grid::new("denoise_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
      ui.label(tr("Suppression strength:"));
      ui.spacing_mut().slider_width = BAR_WIDTH;
      let slider = egui::Slider::new(&mut ai_strength, 0.0..=1.0)
        .show_value(true)
        .custom_formatter(|v, _| format!("{:.0} %", v * 100.0))
        .custom_parser(|s| s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0));
      changed |= ui.add(slider).changed();
      ui.end_row();

      // Any old threshold value (e.g. -45) means "on": the gate is automatic now.
      let mut gate_auto = gate_db > NOISE_GATE_OFF;
      // Shown ticked while the gate is off: that is what turning the gate on gives (the default).
      let mut protect = !gate_auto || gate_db >= NOISE_GATE_AUTO_LOOKAHEAD;
      ui.label(tr("Noise gate:"));
      ui.horizontal(|ui| {
        changed |= ui
          .selectable_value(&mut gate_auto, true, tr("Auto"))
          .on_hover_text(tr(
            "Turns the mic down by 12 dB while you are not speaking. Opens on speech, not on loudness: \
             works the same for a loud or quiet voice. Tuned to never cut your voice, so loud typing \
             sometimes gets through (most often with RNNoise, which removes the least).",
          ))
          .changed();
        changed |= ui
          .selectable_value(&mut gate_auto, false, tr("Off"))
          .on_hover_text(tr("Only the noise suppression runs; nothing is turned down between words."))
          .changed();
      });
      ui.end_row();

      ui.label("");
      ui.add_enabled_ui(gate_auto, |ui| {
        let text = trf("Protect word onsets (+{})", &[&ms_text(AiVoiceIsolator::lookahead_ms(selected))]);
        changed |= ui
          .checkbox(&mut protect, text)
          .on_hover_text(tr(
            "Holds your voice back briefly so the gate is already open when a word starts after a pause. \
             Without it the first syllable can come out quieter. Adds this much delay; \
             only active while the gate is on.",
          ))
          .changed();
      });
      ui.end_row();
      if changed {
        let gate = match (gate_auto, protect) {
          (false, _) => NOISE_GATE_OFF,
          (true, true) => NOISE_GATE_AUTO_LOOKAHEAD,
          (true, false) => NOISE_GATE_AUTO,
        };
        state.update_shm_mic_controls(aec_enabled, ai_strength, gate);
      }
    });
  });
}

/// One line: the delay the chain adds, measured in the DSP; the breakdown on hover. Hidden while
/// no app uses the microphone (nothing to measure).
fn latency_line(ui: &mut Ui, state: &mut AppState) {
  if matches!(state.denoise_status(), DenoiseStatus::Idle(_)) {
    return;
  }
  let l = state.mic_latency();
  let text = match l.measured_ms {
    Some(ms) => trf("After all filters your voice is delayed by {} in total (measured)", &[&ms_text(ms)]),
    None => tr("Delay: measuring, say something...").to_owned(),
  };
  let breakdown = trf(
    "Measured continuously: the raw microphone compared with what apps receive. Right now: echo cancellation {}, noise suppression {}, word-onset protection {}. EQ, leveler and limiter add no delay.",
    &[&ms_text(l.aec_ms), &ms_text(l.denoise_ms), &ms_text(l.lookahead_ms)],
  );
  // Normal size with the status icon: a caption was easy to miss at the top of the page.
  theme::status_line(ui, Severity::Info, &text).on_hover_text(breakdown);
}

/// CPU time of the whole mic chain against the real-time budget of each audio block.
fn dsp_load_row(ui: &mut Ui, load: super::state::DspLoad) {
  let p = theme::palette(ui);
  let color = if load.peak > 1.0 {
    p.err
  } else if load.peak > 0.7 {
    p.warn
  } else {
    p.ok
  };
  ui.horizontal(|ui| {
    ui.label(theme::caption(tr("CPU load:")));
    let value = trf("{} % (peak {} %)", &[&format!("{:.0}", load.avg * 100.0), &format!("{:.0}", load.peak * 100.0)]);
    theme::meter(ui, load.avg, 180.0, color, &value).on_hover_text(tr(
      "Time the mic chain needs per audio block, as a share of the block's length. Above 100 % the block is late: crackles or dropouts. Sustained overload switches DeepFilterNet or DPDFNet to RNNoise (see Log).",
    ));
    let late = RichText::new(trf("late blocks: {}", &[&load.late_blocks])).small();
    ui.label(if load.late_blocks > 0 { late.color(p.warn) } else { late.weak() });
  });
}

const RNNOISE_ABOUT: &str = "RNNoise: light classic model (nnnoiseless), ~0.1 % CPU.\n\
  Voice: stays natural, the model itself never cuts it.\n\
  Keyboard: removed the least (~12-15 dB); loud typing stays audible and can keep the gate open.\n\
  Pick it for a quiet room or a weak CPU.";
const DEEPFILTER_ABOUT: &str = "DeepFilterNet 3: best open-source quality, full-band 48 kHz, ~1 % CPU.\n\
  Keyboard: removed well (~20-30 dB).\n\
  Voice: a very quiet voice under loud typing can lose some soft syllables.\n\
  Switching during a call loads the model (~0.2 s gap).";
const DPDFNET_ABOUT: &str =
  "DPDFNet: strongest keyboard removal (~47-50 dB); the gate almost never lets keys through.\n\
  Voice: best on quiet speech; very loud typing right over a word can thin it out.\n\
  About 3x the CPU of DeepFilterNet 3. Switching during a call loads the model (~0.1 s gap).";

/// Delay the mic gets with this model: its own (measured, see the denoiser's latency test)
/// plus the gate lookahead when word-onset protection is on.
fn total_latency_ms(m: DenoiseModel, ahead: bool) -> f32 {
  AiVoiceIsolator::latency_ms(m) + if ahead { AiVoiceIsolator::lookahead_ms(m) } else { 0.0 }
}

fn ms_text(ms: f32) -> String {
  trf("{} ms", &[&format!("{ms:.0}")])
}

fn latency_breakdown(m: DenoiseModel, ahead: bool) -> String {
  let model = ms_text(AiVoiceIsolator::latency_ms(m));
  if ahead {
    trf(
      "Expected latency: {} = {} model + {} word-onset protection.",
      &[&ms_text(total_latency_ms(m, true)), &model, &ms_text(AiVoiceIsolator::lookahead_ms(m))],
    )
  } else {
    trf("Expected latency: {}.", &[&model])
  }
}

fn model_name(m: DenoiseModel) -> String {
  match m {
    DenoiseModel::RnNoise => "RNNoise".into(),
    DenoiseModel::DeepFilterNet => "DeepFilterNet 3".into(),
    DenoiseModel::DpdfNet => "DPDFNet".into(),
    DenoiseModel::Unknown(v) => trf("unknown model {}", &[&v]),
  }
}

fn eq_section(ui: &mut Ui, state: &mut AppState) {
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::EQUALIZER, EQ_COLOR)), tr("Stage 3: Voice parametric EQ"));
    super::mic_preset_bar::render_preset_bar(ui, state);
    ui.label(theme::caption(tr(
      "Applied after noise suppression, before the boost/limiter. Changes are live and kept across restarts; \"Save as…\" keeps them as a named preset.",
    )));

    let height = super::eq_editor::plot_height(ui, 0.3, 180.0, 340.0);
    if super::eq_editor::band_editor(ui, "mic_eq", &mut state.mic_profile.bands, MAX_EQ_BANDS, EQ_COLOR, &[], height) {
      state.sync_mic_eq_to_shm();
      state.mic_eq_unsaved = true;
    }
    // Save once the slider is released, not on every drag frame.
    if state.mic_eq_unsaved && !ui.ctx().input(|i| i.pointer.any_down()) {
      state.mic_eq_unsaved = false;
      state.apply_mic_eq();
    }
  });
}

fn mic_controls(state: &AppState) -> (bool, f32, i32) {
  state.shm_header().map_or((true, 0.75, NOISE_GATE_AUTO_LOOKAHEAD), |h| {
    (
      h.aec_enabled.load(Ordering::Relaxed),
      h.ai_voice_strength_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      h.noise_gate_threshold_db.load(Ordering::Relaxed),
    )
  })
}
