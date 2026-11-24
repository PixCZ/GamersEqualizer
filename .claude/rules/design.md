---
paths:
  - "crates/gamersequalizer-daemon/src/ui/**"
  - "crates/gamersequalizer-daemon/src/flyout.rs"
  - "crates/gamersequalizer-daemon/src/flyout/**"
---

# Design: how the app looks and reads

One look for the window (egui) and the tray flyout (GDI): Windows 11 / Fluent 2, Segoe UI, orange
accent. UI surfaces and text colors come from `Palette` in `ui/theme.rs` (`palette(ui)` in egui,
`palette_for(dark)` in the flyout): add a token there instead of a literal in a tab or in the flyout's
painting. Named constants for a channel's or brand's own color (`tab_mixer.rs`, `tab_mic.rs`, ...) are
fine; they are drawn through `adapt` / `adapt_for`, which darkens them on the light theme.

## Colors

| Token                       | Use                                                                         |
| --------------------------- | --------------------------------------------------------------------------- |
| `accent`                    | Thin and small things: slider and meter fills, navigation pill, links, check |
| `accent_fill` + `on_accent` | Any area that carries text or a knob: buttons, switch, step circle, selected chips and options, text selection |
| `text`, `text_secondary`    | Text on `card` / `inset`                                                    |
| `text_tertiary`             | Hints, tracks and muted fills. In the light theme it is 3.6:1: not for text that must be read |
| `ok`, `warn`, `err`         | Status color of icons and short text, never a large fill                    |

- **Never put text on `accent`.** In the dark theme it is a bright orange `(255, 140, 64)`: dark text on
  it reads thin and blurry next to the dark window (a bright area bleeds over thin dark strokes).
  `accent_fill` is a darker orange `(181, 71, 0)` with white text (5.4:1). The light theme's accent is
  already dark enough, so both tokens match there (white on it: 4.6:1).
- egui's selection color (`visuals.selection`) is `accent_fill`. An egui `Slider` fills with it, so every
  slider goes through `theme::slider` / `theme::slider_sized`, which put the bright `accent` back. A new
  slider never calls `ui.add(Slider)` directly.
- Text on any fill: at least 4.5:1 in every state. Non-text parts (knob, focus ring, slider fill,
  graph line): at least 3:1 against what is behind them.
- Hover and pressed states darken the fill, in both themes: towards the card in the dark theme, towards
  the text color in the light one. Towards the light card the light theme's fill gets lighter and white
  text drops to 3.7:1. Check a new state in both themes.
- A color or typography change is judged on the real screen in both themes: say what you changed and ask
  for a look. Do not guess at weight or smoothing tweaks; on the flyout button a heavier weight, another
  face and grayscale smoothing all read worse than a darker fill.

## Text

| Where  | Face                                                | Sizes (px)                                           |
| ------ | --------------------------------------------------- | ---------------------------------------------------- |
| Window | Segoe UI Variable (`SegUIVar.ttf`), else Segoe UI   | 14 body and button, 12 small, 18 card title, 28 page title; Cascadia Mono 13 |
| Flyout | Segoe UI through GDI ClearType                      | 14 body and button, 12 caption; icons 16 / 11        |

- Flyout buttons use Segoe UI Variable Text when `C:\Windows\Fonts\SegUIVar.ttf` exists (as the
  window does), else Segoe UI. The `*_PX` constants in `flyout/paint.rs` are pixels.
- Weights: regular for body and buttons, semibold (`seguisb.ttf` in the window) for a name or heading
  line. There is no Segoe UI Medium face: GDI would substitute one of the two.
- Icons: Segoe Fluent Icons in the window (`SegoeIcons.ttf`), Segoe MDL2 Assets in the flyout.
- A smaller text needs a reason. Czech diacritics must render: the face covers Latin Extended-A.
- Every text goes through `tr` / `trf` / `tk` (see `code-style.md`).

## Widgets

- Buttons: `theme::accent_button` for the one primary action of a group, `ui.button` for the rest. The
  flyout draws the same pair itself (`flyout/paint.rs`) from the same tokens; its radius is 4 px
  (`RADIUS`), the window's controls 5 px (`CONTROL_RADIUS`): keep the difference or change both.
- Sliders: `theme::slider`, `theme::slider_sized`, `theme::percent_volume`. Meters and switches:
  `theme::meter`, `theme::toggle`. EQ: `ui/eq_editor.rs`, `ui/eq_graph.rs`.
- A change to the flyout's colors, sizes or states checks the window, and the other way round.
