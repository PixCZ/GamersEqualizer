//! The app's icon (window and tray), drawn in code: equalizer bars on a rounded square.
//! Red variant = some app is recording from a microphone. Warning badge (amber circle with
//! "!", bottom right) = a setup problem (`health`); it combines with either colour.
//! `bar_scale` below 1.0 shortens the bars: the tray shows the microphone level with it (`tray_level`).

/// Bar heights as a fraction of the icon, left to right (an EQ curve).
const BARS: [f32; 4] = [0.42, 0.62, 0.36, 0.52];
const BAR_WIDTH: f32 = 0.12;
const BAR_GAP: f32 = 0.06;
const BAR_BOTTOM: f32 = 0.80;
const CORNER_RADIUS: f32 = 0.22;
/// Samples per pixel axis (anti-aliasing).
const SUPERSAMPLE: u32 = 4;

const NORMAL_BG: [u8; 3] = [32, 32, 36];
const NORMAL_BARS: [u8; 3] = [255, 120, 40];
const MIC_BG: [u8; 3] = [215, 38, 38];
const MIC_BARS: [u8; 3] = [255, 255, 255];

/// Badge: a third of the icon's area, so the "!" reads at 16 px (the tray at 100 % scaling).
/// Center + radius + ring = 1.0: it touches the bottom-right edges without being cut.
const BADGE_CENTER: f32 = 0.62;
const BADGE_RADIUS: f32 = 0.34;
/// Dark ring that separates the badge from the bars under it.
const BADGE_RING: f32 = 0.04;
const BADGE_FILL: [u8; 3] = [255, 196, 0];
const BADGE_RING_COLOR: [u8; 3] = [20, 20, 20];
const BADGE_MARK: [u8; 3] = [20, 20, 20];
/// "!" stroke: half width, top/bottom of the bar, top/bottom of the dot.
const MARK_HALF_WIDTH: f32 = 0.065;
const MARK_TOP: f32 = 0.38;
const MARK_BAR_BOTTOM: f32 = 0.70;
const MARK_DOT_TOP: f32 = 0.77;
const MARK_DOT_BOTTOM: f32 = 0.89;

/// Square RGBA image, `size` x `size`. `bar_scale`: bar heights as a share of the full ones.
pub fn rgba(size: u32, mic_active: bool, warning: bool, bar_scale: f32) -> Vec<u8> {
  let (bg, bars) = if mic_active { (MIC_BG, MIC_BARS) } else { (NORMAL_BG, NORMAL_BARS) };
  let mut out = Vec::with_capacity((size * size * 4) as usize);
  let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
  for py in 0..size {
    for px in 0..size {
      // Sum of the covered samples' colours; uncovered samples are transparent.
      let (mut covered, mut sum) = (0.0f32, [0.0f32; 3]);
      for sy in 0..SUPERSAMPLE {
        for sx in 0..SUPERSAMPLE {
          let x = (px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
          let y = (py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32) / size as f32;
          let color = if warning { badge_color(x, y) } else { None }
            .or_else(|| in_rounded_square(x, y).then_some(if in_bar(x, y, bar_scale) { bars } else { bg }));
          if let Some(c) = color {
            covered += 1.0;
            for (s, v) in sum.iter_mut().zip(c) {
              *s += v as f32;
            }
          }
        }
      }
      for s in sum {
        out.push(if covered > 0.0 { (s / covered).round() as u8 } else { 0 });
      }
      out.push((covered / samples * 255.0).round() as u8);
    }
  }
  out
}

/// Colour of the warning badge at (x, y), None outside it.
fn badge_color(x: f32, y: f32) -> Option<[u8; 3]> {
  let d = ((x - BADGE_CENTER).powi(2) + (y - BADGE_CENTER).powi(2)).sqrt();
  if d > BADGE_RADIUS + BADGE_RING {
    return None;
  }
  if d > BADGE_RADIUS {
    return Some(BADGE_RING_COLOR);
  }
  let in_stroke = (x - BADGE_CENTER).abs() <= MARK_HALF_WIDTH;
  let in_mark =
    in_stroke && ((MARK_TOP..=MARK_BAR_BOTTOM).contains(&y) || (MARK_DOT_TOP..=MARK_DOT_BOTTOM).contains(&y));
  Some(if in_mark { BADGE_MARK } else { BADGE_FILL })
}

fn in_rounded_square(x: f32, y: f32) -> bool {
  let r = CORNER_RADIUS;
  let cx = x.clamp(r, 1.0 - r);
  let cy = y.clamp(r, 1.0 - r);
  (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn in_bar(x: f32, y: f32, scale: f32) -> bool {
  let total = BARS.len() as f32 * BAR_WIDTH + (BARS.len() - 1) as f32 * BAR_GAP;
  let left = (1.0 - total) / 2.0;
  BARS.iter().enumerate().any(|(i, h)| {
    let x0 = left + i as f32 * (BAR_WIDTH + BAR_GAP);
    x >= x0 && x < x0 + BAR_WIDTH && y <= BAR_BOTTOM && y >= BAR_BOTTOM - h * scale
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  const SIZE: u32 = 32;

  #[test]
  fn badge_shows_only_with_warning_and_keeps_mic_colour() {
    // Inside the badge, left of the "!".
    let badge = pixel(&rgba(SIZE, true, true, 1.0), 0.45, 0.62);
    assert_eq!(&badge[..3], &BADGE_FILL);
    let plain = pixel(&rgba(SIZE, true, false, 1.0), 0.45, 0.62);
    assert_ne!(&plain[..3], &BADGE_FILL);
    // Top left is untouched: still the red mic background.
    assert_eq!(&pixel(&rgba(SIZE, true, true, 1.0), 0.3, 0.1)[..3], &MIC_BG);
  }

  #[test]
  fn bars_shorten_with_the_scale() {
    // Inside the tallest bar: near its top, and near its bottom.
    let (top, bottom) = ((0.41, 0.25), (0.41, 0.75));
    assert_eq!(&pixel(&rgba(SIZE, true, false, 1.0), top.0, top.1)[..3], &MIC_BARS);
    assert_eq!(&pixel(&rgba(SIZE, true, false, 0.3), top.0, top.1)[..3], &MIC_BG);
    assert_eq!(&pixel(&rgba(SIZE, true, false, 0.3), bottom.0, bottom.1)[..3], &MIC_BARS);
  }

  fn pixel(img: &[u8], x: f32, y: f32) -> [u8; 4] {
    let (px, py) = ((x * SIZE as f32) as usize, (y * SIZE as f32) as usize);
    let i = (py * SIZE as usize + px) * 4;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
  }
}
