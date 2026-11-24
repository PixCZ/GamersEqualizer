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

| Token                       | Use                                                                                                |
| --------------------------- | -------------------------------------------------------------------------------------------------- |
| `accent`                    | Thin and small things: slider and meter fills, navigation pill, links, check                       |
| `accent_fill` + `on_accent` | Any area with text or a knob: buttons, switch, step circle, selected chips/options, text selection |
| `text`, `text_secondary`    | Text on `card` / `inset`                                                                           |
| `text_tertiary`             | Hints, tracks and muted fills. Light theme is 3.6:1: never for text that must be read              |
| `ok`, `warn`, `err`         | Status color of icons and short text, never a large fill                                           |

- **Never put text on `accent`.** In the dark theme it is a bright orange `(255, 140, 64)`: dark text on
  it reads thin and blurry next to the dark window (a bright area bleeds over thin dark strokes).
  `accent_fill` is a darker orange `(181, 71, 0)` with white text (5.4:1). The light theme's accent is
  already dark enough, so both tokens match there (white on it: 4.6:1).
- egui's selection color (`visuals.selection`) is `accent_fill`. An egui `Slider` fills with it, so every
  slider goes through `theme::slider` / `theme::slider_sized`, which put the bright `accent` back. A new
  slider never calls `ui.add(Slider)` directly.
- **WCAG AA contrast:** text on any fill at least 4.5:1 in every state (normal, hover, pressed, selected).
  Non-text UI parts (knob, focus ring, slider fill, graph line, icons): at least 3:1 against what is
  behind them. Check new channel/brand colors after `adapt` / `adapt_for` in the light theme too (4.5:1
  whenever drawn as text, e.g. balance readouts).
- Hover and pressed states darken the fill, in both themes: towards the card in the dark theme, towards
  the text color in the light one. Towards the light card the light theme's fill gets lighter and white
  text drops to 3.7:1. Check a new state in both themes.
- **Color is never the only cue (WCAG 1.4.1):** status (`ok` / `warn` / `err`), channels, mute and on/off
  always pair color with an icon, shape or label.
- A color or typography change is judged on the real screen in both themes: say what you changed and ask
  for a look. Do not guess at weight or smoothing tweaks; on the flyout button a heavier weight, another
  face and grayscale smoothing all read worse than a darker fill.

## Text

| Where  | Face                                              | Sizes (DIP / px at 96 DPI)                                                   |
| ------ | ------------------------------------------------- | ---------------------------------------------------------------------------- |
| Window | Segoe UI Variable (`SegUIVar.ttf`), else Segoe UI | 14 body and button, 12 small, 18 card title, 28 page title; Cascadia Mono 13 |
| Flyout | Segoe UI through GDI ClearType                    | 14 body and button, 12 caption; icons 16 / 11                                |

- Flyout buttons use Segoe UI Variable Text when `C:\Windows\Fonts\SegUIVar.ttf` exists (as the
  window does), else Segoe UI. The `*_PX` constants in `flyout/paint.rs` are DIPs at 96 DPI and always
  scale through `layout.px(...)` so 125–200 % Windows scaling stays crisp and legible.
- Weights: regular for body and buttons, semibold (`seguisb.ttf` in the window) for a name or heading
  line. There is no Segoe UI Medium face: GDI would substitute one of the two.
- Icons: Segoe Fluent Icons in the window (`SegoeIcons.ttf`), Segoe MDL2 Assets in the flyout.
- A smaller text needs a reason. Czech diacritics must render: the face covers Latin Extended-A.
- Every text goes through `tr` / `trf` / `tk` (see `code-style.md`). Format units with a space (`75 %`,
  `-24 dB`, `12 ms`) and reuse `theme::volume_text` / `theme::level_text` instead of ad-hoc formatting.
- **Self-explanatory UI (nothing to google):** write for a gamer, not an audio engineer. Prefer plain
  labels over bare jargon or acronyms (`Echo cancellation` over `AEC`, `Audio effects` over `APO`, `0 %`
  over `-inf dBFS`). Any setting, slider or technical term that is not obvious on sight (`Q`, `dBFS`,
  sidetone, ChatMix, ducking, ...) gets a short plain-language explanation of what it does and what
  changing it affects — via `theme::caption` or a tooltip (`on_hover_text`), never a mystery control.

## Consistency and widgets

- **Page structure:** every tab follows the same skeleton — `theme::page_header`, then `theme::card`
  (`CARD_GAP` 12, `CARD_PADDING` 16, `CARD_RADIUS` 8) with `theme::card_title`, recessed areas in
  `theme::inset_frame`. Do not build ad-hoc frames or spacing where a `theme` helper exists.
- **Feedback:** banners use `theme::info_bar`, inline statuses use `theme::status_line` with `Severity` —
  never manually colored `ui.label`s for warnings or errors.
- **Buttons:** at most one `theme::accent_button` per logical group (the primary action); `ui.button` for
  the rest. The flyout draws the same pair itself (`flyout/paint.rs`) from the same tokens; its radius is
  4 px (`RADIUS`), the window's controls 5 px (`CONTROL_RADIUS`): keep the difference or change both.
- **Shared controls:** sliders (`theme::slider`, `theme::slider_sized`, `theme::percent_volume`), meters
  and switches (`theme::meter`, `theme::toggle`), EQ (`ui/eq_editor.rs`, `ui/eq_graph.rs`).
- **Accessibility (keyboard & targets):** every interactive control is reachable by keyboard (`Tab`,
  `Enter`/`Space`, `Esc` to dismiss lists/flyout, arrows on sliders/lists) with a visible focus ring, and
  has a hit target of at least 24×24 DIP (WCAG 2.5.8; Fluent 2 controls aim for 32 px height).
- **Window <-> flyout parity:** a change to the flyout's colors, sizes, labels or states checks the
  window, and the other way round.
