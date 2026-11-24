//! Parametric EQ widgets shared by the output EQ tabs and the microphone tab.

use super::theme;
use crate::i18n::{tr, trf};
use crate::output_eq::auto_preamp::{evaluate_bands_response, generate_log_frequencies};
use egui::{Color32, Stroke, Ui};
use egui_plot::{HLine, HoverPosition, Line, Plot, PlotPoint, PlotPoints};
use gamersequalizer_common::profile::BiquadBand;

pub use super::eq_graph::band_editor;

/// Plot height that follows the window: `fraction` of its height, within [min, max].
pub fn plot_height(ui: &Ui, fraction: f32, min: f32, max: f32) -> f32 {
  (ui.ctx().content_rect().height() * fraction).clamp(min, max)
}

/// One curve of `curves_plot`: (legend name, gains in dB per frequency, color, line width).
pub type Curve<'a> = (&'a str, &'a [f32], Color32, f32);

/// Frequency response plot of one band set (20 Hz .. 20 kHz).
pub fn response_plot(ui: &mut Ui, id: &str, bands: &[BiquadBand], color: Color32, height: f32) {
  let freqs = generate_log_frequencies();
  let gains = evaluate_bands_response(bands, &freqs, 48_000.0);
  curves_plot(ui, id, &freqs, &[(tr("Response"), &gains, color, 2.2)], None, height);
}

/// Several response curves on a log-frequency axis, with an optional preamp line.
pub fn curves_plot(ui: &mut Ui, id: &str, freqs: &[f32], curves: &[Curve], preamp_db: Option<f32>, height: f32) {
  let peak = curves.iter().flat_map(|c| c.1.iter()).fold(0.0f32, |m, g| m.max(g.abs())).max(12.0);
  let p = theme::palette(ui);
  let (zero_line, preamp_color) = (p.text_tertiary, p.err);
  let curves: Vec<Curve> = curves.iter().map(|&(n, g, c, w)| (n, g, theme::adapt(ui, c), w)).collect();

  Plot::new(id)
    .height(height)
    .include_x(20f64.log10())
    .include_x(20000f64.log10())
    .include_y(-peak as f64)
    .include_y(peak as f64)
    .allow_drag(false)
    .allow_zoom(false)
    .allow_scroll(false)
    .x_axis_formatter(|val, _range| {
      let f = 10f64.powf(val.value);
      if f >= 1000.0 { format!("{:.1} k", f / 1000.0) } else { format!("{:.0}", f) }
    })
    .y_axis_formatter(|val, _range| format!("{:.0} dB", val.value))
    .label_formatter(|pos| {
      let p = hover_point(pos);
      Some(format!("{:.0} Hz\n{:+.1} dB", 10f64.powf(p.x), p.y))
    })
    .show(ui, |plot_ui| {
      plot_ui.hline(HLine::new("", 0.0).stroke(Stroke::new(1.0_f32, zero_line)));
      if let Some(preamp) = preamp_db.filter(|p| *p != 0.0) {
        plot_ui.hline(
          HLine::new(trf("Volume change {} dB", &[&format!("{preamp:+.1}")]), preamp as f64)
            .stroke(Stroke::new(1.0_f32, preamp_color)),
        );
      }
      for &(name, gains, color, width) in &curves {
        let pts: PlotPoints = freqs.iter().zip(gains).map(|(&f, &g)| [(f as f64).log10(), g as f64]).collect();
        plot_ui.line(Line::new(name, pts).stroke(Stroke::new(width, color)));
      }
    });
}

/// Plot position under the cursor, whether or not it is near a data point.
pub fn hover_point(pos: &HoverPosition<'_>) -> PlotPoint {
  match *pos {
    HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => position,
  }
}

/// Searchable profile combo. Returns Some(choice) when the user picked something;
/// choice None = the `none_label` entry. Names are shown through `tr` (built-in profiles
/// are translation keys, others pass through) but returned as stored.
pub fn profile_picker(
  ui: &mut Ui,
  id: &str,
  selected: Option<&str>,
  names: &[&str],
  none_label: &str,
  filter: &mut String,
  width: f32,
) -> Option<Option<String>> {
  let mut picked = None;
  egui::ComboBox::from_id_salt(id)
    .selected_text(super::truncate_chars(selected.map_or(none_label, tr), 34))
    .width(width)
    .height(320.0)
    // The default closes on any click, also into the filter field: close only on a pick or outside.
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show_ui(ui, |ui| {
      ui.add(egui::TextEdit::singleline(filter).hint_text(tr("Filter...")).desired_width(width));
      if ui.selectable_label(selected.is_none(), none_label).clicked() {
        picked = Some(None);
      }
      let needle = filter.to_lowercase();
      for &name in names.iter().filter(|n| needle.is_empty() || tr(n).to_lowercase().contains(&needle)) {
        if ui.selectable_label(selected == Some(name), tr(name)).clicked() {
          picked = Some(Some(name.to_string()));
        }
      }
      if picked.is_some() {
        ui.close();
      }
    });
  picked
}
