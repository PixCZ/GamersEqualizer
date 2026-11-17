//! Visual parametric EQ editor: the response graph with one dot per filter (drag it to
//! move frequency and gain, mouse wheel over it for width, double-click the graph to add),
//! a row of filter chips, and the settings of the selected filter below.

use egui::{Align2, Color32, CursorIcon, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2};
use gamersequalizer_common::profile::{BiquadBand, FilterType, SLOPES_DB, slots_used};

use super::eq_editor::Curve;
use super::theme;
use crate::i18n::{tk, tr, trf};
use crate::output_eq::auto_preamp::{evaluate_bands_response, generate_log_frequencies};

const F_MIN: f32 = 20.0;
const F_MAX: f32 = 20000.0;
const SAMPLE_RATE: f32 = 48_000.0;
/// Pointer distance (points) that still grabs a dot.
const GRAB_RADIUS: f32 = 16.0;
const DOT_RADIUS: f32 = 8.0;
/// Room for the dB labels (left) and Hz labels (bottom).
const AXIS_LEFT: f32 = 34.0;
const AXIS_BOTTOM: f32 = 16.0;
/// Width of a bell in octaves, the slider range.
const WIDTH_OCT_RANGE: std::ops::RangeInclusive<f32> = 0.1..=5.0;
/// Mouse wheel: Q change per scrolled point (about 8 % per wheel notch).
const WHEEL_Q_PER_POINT: f32 = 1.0015;
const GRID_FREQS: [f32; 10] = [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0];
const DOT_COLORS: [Color32; 8] = [
  Color32::from_rgb(255, 170, 60),
  Color32::from_rgb(90, 200, 255),
  Color32::from_rgb(140, 230, 110),
  Color32::from_rgb(240, 110, 180),
  Color32::from_rgb(250, 220, 80),
  Color32::from_rgb(170, 140, 255),
  Color32::from_rgb(80, 220, 190),
  Color32::from_rgb(255, 120, 100),
];
const DISABLED_COLOR: Color32 = Color32::from_gray(110);

/// Pixel <-> (frequency, dB) mapping of the plot area.
struct Axes {
  plot: Rect,
  range_db: f32,
}

/// The whole editor: graph, filter chips, selected filter settings. `max_slots` is what the
/// audio engine holds (a 24 dB/oct cut takes 2, a 48 dB/oct cut 4). `background` curves (on
/// the `generate_log_frequencies` grid) are drawn thin behind the edited one.
/// Returns true when the bands changed.
pub fn band_editor(
  ui: &mut Ui,
  id: &str,
  bands: &mut Vec<BiquadBand>,
  max_slots: usize,
  color: Color32,
  background: &[Curve],
  height: f32,
) -> bool {
  let sel_id = egui::Id::unique(id).with("selected");
  let mut selected: Option<usize> =
    ui.data(|d| d.get_temp::<Option<usize>>(sel_id)).flatten().filter(|&i| i < bands.len());

  let mut changed = graph(ui, id, bands, max_slots, color, background, height, &mut selected);
  ui.label(theme::caption(tr(
    "Drag a dot to move it · mouse wheel over a dot = narrower / wider · double-click the graph to add a filter",
  )));
  changed |= chips(ui, bands, max_slots, &mut selected);
  if let Some(i) = selected {
    let mut remove = false;
    let others = slots_used(&bands_without(bands, i));
    if let Some(band) = bands.get_mut(i) {
      changed |= details(ui, id, i, band, max_slots.saturating_sub(others), &mut remove);
    }
    if remove && i < bands.len() {
      bands.remove(i);
      selected = None;
      changed = true;
    }
  }

  ui.data_mut(|d| d.insert_temp(sel_id, selected));
  changed
}

#[expect(clippy::too_many_arguments, reason = "private helper: `band_editor`'s arguments plus the selection it keeps")]
fn graph(
  ui: &mut Ui,
  id: &str,
  bands: &mut Vec<BiquadBand>,
  max_slots: usize,
  color: Color32,
  background: &[Curve],
  height: f32,
  selected: &mut Option<usize>,
) -> bool {
  let mut changed = false;
  let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click_and_drag());
  let peak = bands.iter().filter(|b| b.filter_type.uses_gain()).fold(0.0f32, |m, b| m.max(b.gain_db.abs()));
  let axes = Axes {
    plot: Rect::from_min_max(rect.min + Vec2::new(AXIS_LEFT, 4.0), rect.max - Vec2::new(6.0, AXIS_BOTTOM)),
    range_db: if peak > 11.0 { 24.0 } else { 12.0 },
  };

  // --- Input (before drawing, so the frame shows the new state). ---
  let drag_id = egui::Id::unique(id).with("dragging");
  let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
  let hovered = pointer.and_then(|p| nearest_dot(&axes, bands, p));

  if resp.drag_started() {
    let grabbed = resp.interact_pointer_pos().and_then(|p| nearest_dot(&axes, bands, p));
    ui.data_mut(|d| d.insert_temp(drag_id, grabbed));
    if grabbed.is_some() {
      *selected = grabbed;
    }
  }
  let dragging: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)).flatten();
  if let (true, Some(i), Some(p)) = (resp.dragged(), dragging, resp.interact_pointer_pos())
    && let Some(band) = bands.get_mut(i)
  {
    band.frequency = axes.freq_at(p.x).round().clamp(F_MIN, F_MAX);
    if band.filter_type.uses_gain() {
      band.gain_db = ((axes.db_at(p.y) * 2.0).round() / 2.0).clamp(-axes.range_db, axes.range_db);
    }
    changed = true;
  }
  if resp.drag_stopped() {
    ui.data_mut(|d| d.insert_temp::<Option<usize>>(drag_id, None));
  }
  if resp.clicked()
    && let Some(i) = hovered
  {
    *selected = Some(i);
  }
  if resp.double_clicked()
    && hovered.is_none()
    && slots_used(bands) < max_slots
    && let Some(p) = resp.interact_pointer_pos()
  {
    bands.push(BiquadBand {
      frequency: axes.freq_at(p.x).round().clamp(F_MIN, F_MAX),
      gain_db: ((axes.db_at(p.y) * 2.0).round() / 2.0).clamp(-12.0, 12.0),
      q: 1.0,
      ..BiquadBand::default()
    });
    *selected = Some(bands.len() - 1);
    changed = true;
  }
  if let Some(i) = hovered.filter(|_| resp.hovered()) {
    ui.ctx().set_cursor_icon(if dragging.is_some() { CursorIcon::Grabbing } else { CursorIcon::Grab });
    let wheel = ui.input(|inp| inp.smooth_scroll_delta.y);
    if let Some(band) = bands.get_mut(i).filter(|b| wheel != 0.0 && !b.is_cut()) {
      // Wheel up = wider = lower Q. Eaten, so the page doesn't scroll at the same time.
      band.q = (band.q / WHEEL_Q_PER_POINT.powf(wheel)).clamp(0.1, 30.0);
      ui.input_mut(|inp| inp.smooth_scroll_delta = Vec2::ZERO);
      *selected = Some(i);
      changed = true;
    }
  }

  // --- Drawing. ---
  let pal = theme::palette(ui);
  let painter = ui.painter_at(rect);
  painter.rect_filled(rect, theme::CONTROL_RADIUS, pal.inset);
  draw_grid(&painter, &axes, &pal);
  let color = theme::adapt(ui, color);

  let freqs = generate_log_frequencies();
  let clip = ui.painter_at(axes.plot);
  for &(_, gains, c, width) in background {
    let c = theme::adapt(ui, c);
    clip.add(Shape::line(axes.curve_points(&freqs, gains), Stroke::new(width.min(1.4), c.gamma_multiply(0.6))));
  }
  if let Some(band) = selected.and_then(|i| bands.get(i)).filter(|b| b.enabled) {
    let own = evaluate_bands_response(std::slice::from_ref(band), &freqs, SAMPLE_RATE);
    let c = theme::adapt(ui, dot_color(selected.unwrap_or(0), band));
    clip.add(Shape::line(axes.curve_points(&freqs, &own), Stroke::new(1.0_f32, c.gamma_multiply(0.7))));
  }
  let total = evaluate_bands_response(bands, &freqs, SAMPLE_RATE);
  clip.add(Shape::line(axes.curve_points(&freqs, &total), Stroke::new(2.4_f32, color)));

  for (i, band) in bands.iter().enumerate() {
    let c = theme::adapt(ui, dot_color(i, band));
    let center = axes.dot_pos(band);
    let is_sel = *selected == Some(i);
    let radius = if is_sel || hovered == Some(i) { DOT_RADIUS + 2.0 } else { DOT_RADIUS };
    let outline =
      if is_sel { Stroke::new(2.0_f32, pal.text) } else { Stroke::new(1.0_f32, Color32::from_black_alpha(90)) };
    painter.circle(center, radius, c, outline);
    painter.text(center, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(12.0), Color32::BLACK);
  }
  if let Some(band) = hovered.and_then(|i| bands.get(i)) {
    let pos = axes.dot_pos(band) + Vec2::new(0.0, -DOT_RADIUS - 6.0);
    let text = describe(band);
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let mut box_rect =
      Rect::from_center_size(pos - Vec2::new(0.0, galley.size().y / 2.0), galley.size() + Vec2::splat(8.0));
    box_rect = box_rect.translate(Vec2::new(
      (rect.left() - box_rect.left()).max(0.0) + (rect.right() - box_rect.right()).min(0.0),
      (rect.top() - box_rect.top()).max(0.0),
    ));
    painter.rect_filled(box_rect, 3.0, Color32::from_black_alpha(220));
    painter.galley(box_rect.min + Vec2::splat(4.0), galley, Color32::WHITE);
  }
  changed
}

fn chips(ui: &mut Ui, bands: &mut Vec<BiquadBand>, max_slots: usize, selected: &mut Option<usize>) -> bool {
  let mut changed = false;
  ui.horizontal_wrapped(|ui| {
    if bands.is_empty() {
      ui.label(theme::caption(tr("No filters: the sound passes unchanged.")));
    }
    for (i, band) in bands.iter().enumerate() {
      let is_selected = *selected == Some(i);
      let text = RichText::new(format!("● {}  {}", i + 1, describe(band)));
      // On the accent selection fill the band color is unreadable: a selected chip keeps the
      // theme's text-on-accent color, the others show the band color.
      let text = if is_selected { text } else { text.color(theme::adapt(ui, dot_color(i, band))) };
      if ui.selectable_label(is_selected, text).clicked() {
        *selected = if *selected == Some(i) { None } else { Some(i) };
      }
    }
    let free = max_slots.saturating_sub(slots_used(bands));
    let add = ui
      .add_enabled(free > 0, egui::Button::new(format!("➕  {}", tr("Add filter"))))
      .on_disabled_hover_text(tr("Full. A 24 dB/oct cut takes the room of 2 filters, a 48 dB/oct cut 4."));
    if add.clicked() {
      bands.push(BiquadBand { q: 1.0, ..BiquadBand::default() });
      *selected = Some(bands.len() - 1);
      changed = true;
    }
  });
  changed
}

/// Settings of one filter. `room` = slots this filter may take. Sets `remove` on 🗑.
fn details(ui: &mut Ui, id: &str, i: usize, band: &mut BiquadBand, room: usize, remove: &mut bool) -> bool {
  let mut changed = false;
  let c = theme::adapt(ui, dot_color(i, band));
  theme::inset_frame(ui).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
    ui.set_width(ui.available_width());
    ui.horizontal_wrapped(|ui| {
      ui.label(theme::strong(format!("● {}", trf("Filter {}", &[&(i + 1)])), 14.0).color(c));
      egui::ComboBox::from_id_salt(format!("{id}_type"))
        .selected_text(tr(friendly_name(&band.filter_type)))
        .width(170.0)
        .show_ui(ui, |ui| {
          for ft in FilterType::ALL {
            if ui
              .selectable_label(band.filter_type == ft, tr(friendly_name(&ft)))
              .on_hover_text(tr(type_help(&ft)))
              .clicked()
              && band.filter_type != ft
            {
              band.filter_type = ft;
              if band.is_cut() {
                band.q = 0.707;
                band.slope_db = if room >= 2 { 24 } else { 12 };
              }
              changed = true;
            }
          }
        });
      changed |=
        ui.checkbox(&mut band.enabled, tr("On")).on_hover_text(tr("Switch off to hear the difference")).changed();
      if ui.button(format!("🗑  {}", tr("Remove"))).clicked() {
        *remove = true;
      }
    });

    ui.horizontal_wrapped(|ui| {
      ui.label(tr("Frequency:"));
      changed |= value_slider(ui, &mut band.frequency, F_MIN..=F_MAX, " Hz", 0, true);
      if band.filter_type.uses_gain() {
        ui.label(tr("Gain:"));
        changed |= value_slider(ui, &mut band.gain_db, -24.0..=24.0, " dB", 1, false);
      }
    });

    ui.horizontal_wrapped(|ui| {
      if band.is_cut() {
        ui.label(tr("Steepness:")).on_hover_text(tr("How fast the sound beyond the frequency disappears."));
        for slope in SLOPES_DB {
          let stages = BiquadBand { slope_db: slope, ..band.clone() }.stages();
          let label = match slope {
            12 => tr("gentle (12 dB/oct)"),
            24 => tr("steep (24)"),
            _ => tr("very steep (48)"),
          };
          let fits = stages <= room;
          let r = ui
            .add_enabled(fits, egui::Button::selectable(band.slope_db == slope, label))
            .on_disabled_hover_text(tr("No room left: remove a filter first."));
          if r.clicked() && band.slope_db != slope {
            band.slope_db = slope;
            changed = true;
          }
        }
      } else if matches!(band.filter_type, FilterType::Peaking | FilterType::Notch) {
        ui.label(tr("Width:")).on_hover_text(tr("Narrow touches one tone, wide a whole region."));
        let mut width = width_octaves(band.q);
        if value_slider(ui, &mut width, WIDTH_OCT_RANGE, " oct", 2, true) {
          band.q = q_for_width(width).clamp(0.1, 30.0);
          changed = true;
        }
      } else {
        ui.label(tr("Slope (Q):")).on_hover_text(tr("Higher = a sharper step into the shelf."));
        changed |= value_slider(ui, &mut band.q, 0.3..=2.0, "", 2, false);
      }
    });
  });
  changed
}

impl Axes {
  fn x_of(&self, f: f32) -> f32 {
    let t = (f.clamp(F_MIN, F_MAX) / F_MIN).ln() / (F_MAX / F_MIN).ln();
    self.plot.left() + t * self.plot.width()
  }

  fn freq_at(&self, x: f32) -> f32 {
    let t = ((x - self.plot.left()) / self.plot.width()).clamp(0.0, 1.0);
    F_MIN * (F_MAX / F_MIN).powf(t)
  }

  fn y_of(&self, db: f32) -> f32 {
    self.plot.center().y - db / self.range_db * self.plot.height() * 0.5
  }

  fn db_at(&self, y: f32) -> f32 {
    (self.plot.center().y - y) / (self.plot.height() * 0.5) * self.range_db
  }

  /// Gain filters sit at their gain, cuts and notches on the 0 dB line (they move sideways).
  fn dot_pos(&self, band: &BiquadBand) -> Pos2 {
    let db = if band.filter_type.uses_gain() { band.gain_db } else { 0.0 };
    Pos2::new(self.x_of(band.frequency), self.y_of(db.clamp(-self.range_db, self.range_db)))
  }

  /// Curve points, clamped a bit past the plot edge (a cut goes to -inf dB).
  fn curve_points(&self, freqs: &[f32], gains: &[f32]) -> Vec<Pos2> {
    let limit = self.range_db * 1.2;
    freqs.iter().zip(gains).map(|(&f, &g)| Pos2::new(self.x_of(f), self.y_of(g.clamp(-limit, limit)))).collect()
  }
}

fn draw_grid(painter: &egui::Painter, axes: &Axes, pal: &theme::Palette) {
  let grid = Stroke::new(1.0_f32, pal.grid);
  let label = pal.text_tertiary;
  let font = FontId::proportional(12.0);
  let plot = axes.plot;
  for f in GRID_FREQS {
    let x = axes.x_of(f);
    painter.line_segment([Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())], grid);
    let text = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
    painter.text(Pos2::new(x, plot.bottom() + 2.0), Align2::CENTER_TOP, text, font.clone(), label);
  }
  let step = axes.range_db / 2.0;
  for k in -2..=2 {
    let db = k as f32 * step;
    let y = axes.y_of(db);
    let stroke = if k == 0 { Stroke::new(1.0_f32, pal.text_tertiary) } else { grid };
    painter.line_segment([Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], stroke);
    painter.text(Pos2::new(plot.left() - 4.0, y), Align2::RIGHT_CENTER, format!("{db:+.0}"), font.clone(), label);
  }
}

/// A slider that writes `value` only when the user moves it. egui clamps a slider value to its
/// range and rounds it to `decimals` on every frame, which silently changed presets
/// (Q 0.541 -> 0.54), so switching presets always asked to save changes nobody made.
fn value_slider(
  ui: &mut Ui,
  value: &mut f32,
  range: std::ops::RangeInclusive<f32>,
  suffix: &str,
  decimals: usize,
  logarithmic: bool,
) -> bool {
  let untouched = shown_value(*value, &range, decimals);
  let mut v = *value;
  ui.add(egui::Slider::new(&mut v, range).logarithmic(logarithmic).suffix(suffix).max_decimals(decimals));
  if v == untouched {
    return false;
  }
  *value = v;
  true
}

/// What egui turns `value` into just by showing it: clamped, rounded to `decimals`.
fn shown_value(value: f32, range: &std::ops::RangeInclusive<f32>, decimals: usize) -> f32 {
  egui::emath::round_to_decimals(value.clamp(*range.start(), *range.end()) as f64, decimals) as f32
}

fn nearest_dot(axes: &Axes, bands: &[BiquadBand], p: Pos2) -> Option<usize> {
  bands
    .iter()
    .enumerate()
    .map(|(i, b)| (i, axes.dot_pos(b).distance(p)))
    .filter(|&(_, d)| d <= GRAB_RADIUS)
    .min_by(|a, b| a.1.total_cmp(&b.1))
    .map(|(i, _)| i)
}

fn bands_without(bands: &[BiquadBand], skip: usize) -> Vec<BiquadBand> {
  bands.iter().enumerate().filter(|&(i, _)| i != skip).map(|(_, b)| b.clone()).collect()
}

fn dot_color(i: usize, band: &BiquadBand) -> Color32 {
  if !band.enabled {
    return DISABLED_COLOR;
  }
  DOT_COLORS.get(i % DOT_COLORS.len()).copied().unwrap_or(Color32::WHITE)
}

fn friendly_name(ft: &FilterType) -> &'static str {
  match ft {
    FilterType::Peaking => tk("Bell (boost / cut)"),
    FilterType::HighPass => tk("Low cut"),
    FilterType::LowPass => tk("High cut"),
    FilterType::LowShelf => tk("Low shelf (bass)"),
    FilterType::HighShelf => tk("High shelf (treble)"),
    FilterType::Notch => tk("Notch (remove one tone)"),
    FilterType::Unknown(_) => tk("Newer filter (passes unchanged)"),
  }
}

fn type_help(ft: &FilterType) -> &'static str {
  match ft {
    FilterType::Peaking => tk("Boosts or cuts the area around the frequency."),
    FilterType::HighPass => tk("Removes everything below the frequency (rumble, desk bumps, cable noise)."),
    FilterType::LowPass => tk("Removes everything above the frequency (hiss)."),
    FilterType::LowShelf => tk("Raises or lowers everything below the frequency."),
    FilterType::HighShelf => tk("Raises or lowers everything above the frequency."),
    FilterType::Notch => tk("Removes one tone, e.g. 50 Hz mains hum."),
    FilterType::Unknown(_) => tk(
      "Made by a newer version of the app. This version passes the sound through it unchanged and keeps it as it is.",
    ),
  }
}

/// Short text for chips and the hover label.
fn describe(band: &BiquadBand) -> String {
  let f = if band.frequency >= 1000.0 {
    format!("{:.1} kHz", band.frequency / 1000.0)
  } else {
    format!("{:.0} Hz", band.frequency)
  };
  let off = if band.enabled { "" } else { tr(" (off)") };
  let gain = format!("{:+.1}", band.gain_db);
  let text = match band.filter_type {
    FilterType::HighPass => trf("Low cut {}, {} dB/oct", &[&f, &band.slope_db]),
    FilterType::LowPass => trf("High cut {}, {} dB/oct", &[&f, &band.slope_db]),
    FilterType::Notch => trf("Notch {}", &[&f]),
    FilterType::LowShelf => trf("Bass {} dB below {}", &[&gain, &f]),
    FilterType::HighShelf => trf("Treble {} dB above {}", &[&gain, &f]),
    FilterType::Peaking => trf("{} dB at {}", &[&gain, &f]),
    FilterType::Unknown(_) => tr("Newer filter (passes unchanged)").to_owned(),
  };
  format!("{text}{off}")
}

/// Bandwidth in octaves of a bell with quality `q` (RBJ cookbook relation).
fn width_octaves(q: f32) -> f32 {
  2.0 / std::f32::consts::LN_2 * (1.0 / (2.0 * q.max(0.01))).asinh()
}

fn q_for_width(octaves: f32) -> f32 {
  1.0 / (2.0 * (std::f32::consts::LN_2 / 2.0 * octaves.max(0.01)).sinh())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn width_and_q_convert_both_ways() {
    for q in [0.3f32, 0.707, 1.0, 1.414, 4.0, 10.0] {
      assert!((q_for_width(width_octaves(q)) - q).abs() < 1e-3 * q, "q {q}");
    }
    assert!((width_octaves(1.414) - 1.0).abs() < 0.01, "Q 1.414 is one octave wide");
  }

  #[test]
  fn showing_a_slider_does_not_count_as_an_edit() {
    // egui rounds 0.541 to 0.54 when it draws the slider: that is not the user editing.
    assert_eq!(shown_value(0.541, &(0.3..=2.0), 2), 0.54);
    assert_eq!(shown_value(12.0, &(0.3..=2.0), 2), 2.0);
    assert_eq!(shown_value(1234.0, &(20.0..=20000.0), 0), 1234.0);
  }

  #[test]
  fn axes_map_frequency_and_db_both_ways() {
    let axes = Axes { plot: Rect::from_min_size(Pos2::new(10.0, 0.0), Vec2::new(300.0, 200.0)), range_db: 12.0 };
    for f in [20.0f32, 80.0, 1000.0, 12000.0, 20000.0] {
      assert!((axes.freq_at(axes.x_of(f)) - f).abs() < f * 1e-3);
    }
    assert!((axes.db_at(axes.y_of(-6.0)) + 6.0).abs() < 1e-3);
    assert!((axes.y_of(0.0) - 100.0).abs() < 1e-3);
  }
}
