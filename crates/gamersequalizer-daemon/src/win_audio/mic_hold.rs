//! Who moved the Windows mic volume, and holding the voice leveler while an app keeps moving it.
//!
//! With the leveler on, the Windows volume is only the input sensitivity: the leveler evens out
//! every change, so an app's own automatic gain control (Teams, Zoom) never hears its step and
//! keeps raising the volume, up to 100 %. A second change from an app within a minute, during one
//! recording, holds the leveler (`mic_leveler_hold`: its gain stays, it no longer evens out the
//! input) until that app stops recording: the app's control then hears itself and settles. An app
//! that does it in two recordings goes on a list and is held from the start next time.
//! Spec: `docs/mic-leveler-apps-spec.md`.

use super::chatmix::ChatMixController;
use crate::ui::audio_settings::{VOLUME_APP_LISTED, VolumeApp, note_volume_app, volume_apps};
use gamersequalizer_common::shm::ShmHeader;
use log::info;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// Moves closer than this are one change, however long it lasts (a slider drag, a key held, an
/// app's smooth ramp): only this much calm ends it.
const CHANGE_GAP: Duration = Duration::from_secs(2);
/// A second change from an app this soon after its first one, in one recording, is a fight.
const FIGHT_WINDOW: Duration = Duration::from_secs(60);
/// Input this recent, with the recording app in front, means the user moved its slider.
const USER_INPUT: Duration = Duration::from_millis(1500);
/// Windows where a person moves the mic volume by hand: Settings, the Sound control panel
/// (`mmsys.cpl` runs in rundll32), the Xbox Game Bar, and this app (window and tray flyout).
const VOLUME_WINDOWS: [&str; 5] =
  ["SystemSettings.exe", "rundll32.exe", "GameBar.exe", "XboxGameBarWidgets.exe", "gamersequalizer.exe"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
  User,
  App,
}

/// The leveler is held: by whom and until when.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
  /// The one app that recorded when it began, or the listed app; None when several recorded.
  pub app: Option<String>,
  /// Apps recording when it began: without `app`, it ends once none of them records.
  pub apps: Vec<String>,
  /// Windows volume to go back to at the end (before the recording); None when unknown or the
  /// user moved it during the recording (theirs stays).
  pub restore: Option<f32>,
  /// Held from the start, for an app on the list.
  pub listed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
  /// A change ended (`CHANGE_GAP` of calm); `scene` says what was in front and recording.
  Moved {
    from: f32,
    to: f32,
    who: Who,
    scene: String,
  },
  Held(Held),
  Released(Held),
}

/// What the service sees once per tick (~250 ms).
pub struct Tick<'a> {
  pub now: Instant,
  /// Windows mic volume now (0..1).
  pub level: f32,
  /// Someone other than this service moved it since the last tick, from this level.
  pub moved_from: Option<f32>,
  /// Apps recording, as `mic_usage::recording_apps` names them.
  pub recording: &'a [String],
  /// Apps on the list (held from the start).
  pub listed: &'a [String],
}

/// The change in progress.
#[derive(Debug)]
struct Change {
  last: Instant,
  who: Who,
  from: f32,
  scene: String,
}

#[derive(Debug, Default)]
pub struct MicHold {
  change: Option<Change>,
  /// The first change from an app in this recording, while no second one came within `FIGHT_WINDOW`.
  first_app: Option<Instant>,
  /// The Windows volume at the last tick nobody recorded: where a hold's end goes back to.
  /// shortcut: an app's first move seen a tick before its recording shows up in Windows counts as
  /// the user's and becomes this level; following the recording's start time would close that.
  idle_level: Option<f32>,
  /// The user moved the volume during this recording.
  user_moved: bool,
  held: Option<Held>,
}

impl MicHold {
  pub fn held(&self) -> Option<&Held> {
    self.held.as_ref()
  }

  /// `foreground` (exe name) and `input_ago` are asked only when the volume moved, the input
  /// time only while the app that records is in front.
  pub fn tick(
    &mut self,
    t: &Tick,
    foreground: impl FnOnce() -> Option<String>,
    input_ago: impl FnOnce() -> Option<Duration>,
  ) -> Vec<Event> {
    let mut events = vec![];
    if let Some(c) = self.change.take_if(|c| t.now.duration_since(c.last) > CHANGE_GAP) {
      events.push(Event::Moved { from: c.from, to: t.moved_from.unwrap_or(t.level), who: c.who, scene: c.scene });
    }
    if let Some(held) = self.held.take_if(|h| match &h.app {
      Some(app) => !records(t.recording, app),
      None => !h.apps.iter().any(|app| records(t.recording, app)),
    }) {
      self.first_app = None;
      events.push(Event::Released(held));
    }
    if t.recording.is_empty() {
      (self.first_app, self.user_moved) = (None, false);
    }
    if self.held.is_none()
      && let Some(app) = t.listed.iter().find(|app| records(t.recording, app))
    {
      self.hold(t, Some(app.clone()), true, &mut events);
    }
    if let Some(from) = t.moved_from {
      self.moved(t, from, foreground, input_ago, &mut events);
    }
    if t.recording.is_empty() {
      self.idle_level = Some(t.level);
    }
    events
  }

  fn moved(
    &mut self,
    t: &Tick,
    from: f32,
    foreground: impl FnOnce() -> Option<String>,
    input_ago: impl FnOnce() -> Option<Duration>,
    events: &mut Vec<Event>,
  ) {
    let who = match &mut self.change {
      Some(c) => {
        c.last = t.now;
        c.who
      }
      None => {
        let front = foreground();
        let who = who(front.as_deref(), t.recording, input_ago);
        let mut scene = format!("foreground {}", front.as_deref().unwrap_or("unknown"));
        if !t.recording.is_empty() {
          scene += &format!(", recording {}", t.recording.join(", "));
        }
        self.change = Some(Change { last: t.now, who, from, scene });
        if who == Who::App && self.held.is_none() {
          match self.first_app {
            Some(first) if t.now.duration_since(first) <= FIGHT_WINDOW => {
              let app = match t.recording {
                [one] => Some(one.clone()),
                _ => None,
              };
              self.hold(t, app, false, events);
            }
            _ => self.first_app = Some(t.now),
          }
        }
        who
      }
    };
    if who == Who::User && !t.recording.is_empty() {
      self.user_moved = true;
      if let Some(held) = &mut self.held {
        held.restore = None;
      }
    }
  }

  fn hold(&mut self, t: &Tick, app: Option<String>, listed: bool, events: &mut Vec<Event>) {
    let restore = self.idle_level.filter(|_| !self.user_moved);
    let held = Held { app, apps: t.recording.to_vec(), restore, listed };
    events.push(Event::Held(held.clone()));
    self.held = Some(held);
  }
}

/// Who moved the volume. Nobody records: the user (no app has a reason to). Someone records: the
/// user if a volume window is in front, or the recording app with input in the last 1.5 s (its
/// own settings); else an app, since without a window in front nobody moves it by hand.
/// Not by input alone: a gamer types and clicks all the time.
pub fn who(foreground: Option<&str>, recording: &[String], input_ago: impl FnOnce() -> Option<Duration>) -> Who {
  let Some(front) = foreground.filter(|_| !recording.is_empty()) else {
    return if recording.is_empty() { Who::User } else { Who::App };
  };
  if VOLUME_WINDOWS.iter().any(|w| w.eq_ignore_ascii_case(front)) {
    return Who::User;
  }
  if recording.iter().any(|app| same_app(app, front)) && input_ago().is_some_and(|ago| ago <= USER_INPUT) {
    Who::User
  } else {
    Who::App
  }
}

fn records(recording: &[String], app: &str) -> bool {
  recording.iter().any(|r| r.eq_ignore_ascii_case(app))
}

/// A recording app's name is its exe (`Zoom.exe`) or its package's short name (`MSTeams` for
/// `ms-teams.exe`): compared on letters and digits, without `.exe`.
/// shortcut: a package whose name differs from its exe in more than that is not recognized in
/// front (its own slider then counts as the app's); matching its exe path would close that.
fn same_app(recording: &str, exe: &str) -> bool {
  let key = |s: &str| {
    let s = s.to_lowercase();
    s.strip_suffix(".exe").unwrap_or(&s).chars().filter(char::is_ascii_alphanumeric).collect::<String>()
  };
  key(recording) == key(exe)
}

/// How long ago the last keyboard or mouse input was, system-wide: its time only (no keys, no
/// hooks), the same call screen savers use. Asked only as described at `who`.
fn input_ago() -> Option<Duration> {
  let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
  // SAFETY: a local struct with its size set, as the call requires.
  let ok = unsafe { GetLastInputInfo(&mut info) }.as_bool();
  // SAFETY: no arguments; the clock `dwTime` is on (wraps every 49 days, as does the difference).
  let now = unsafe { GetTickCount() };
  ok.then(|| Duration::from_millis(u64::from(now.wrapping_sub(info.dwTime))))
}

/// Service side: once per mic tick (~250 ms), after `ChatMixController::sync_mic`.
#[derive(Debug, Default)]
pub struct MicHoldLink {
  hold: MicHold,
  /// Listed apps, read again when the recording apps change (the window may have removed one).
  listed: Vec<String>,
  recording: Option<Vec<String>>,
}

impl MicHoldLink {
  /// Only with the leveler on and working (not bypassed, mic not muted): otherwise a move of the
  /// Windows volume is a move of the mic slider, as without the leveler.
  pub fn tick(
    &mut self,
    h: &ShmHeader,
    chatmix: &mut ChatMixController,
    mic_id: Option<&str>,
    foreground: impl FnOnce() -> Option<String>,
    dir: &Path,
  ) {
    let r = Ordering::Relaxed;
    let moved_from = chatmix.mic_moved_from.take();
    let on = mic_id.is_some() && h.mic_leveler_enabled.load(r) && !h.master_bypass.load(r) && !chatmix.mic.muted;
    if !on {
      if self.hold.held().is_some() {
        info!("[MicVol] voice leveler off, bypassed or the mic muted: no longer held");
      }
      self.switch_mic();
      h.mic_leveler_hold.store(false, r);
      return;
    }
    let recording = crate::mic_usage::recording_apps();
    if self.recording.as_ref() != Some(&recording) {
      self.listed = volume_apps(dir).into_iter().filter(VolumeApp::listed).map(|a| a.app).collect();
      self.recording = Some(recording.clone());
    }
    let level = chatmix.mic.volume.min(1.0);
    let tick = Tick { now: Instant::now(), level, moved_from, recording: &recording, listed: &self.listed };
    for event in self.hold.tick(&tick, foreground, input_ago) {
      self.on_event(event, chatmix, mic_id, dir);
    }
    h.mic_leveler_hold.store(self.hold.held().is_some(), r);
  }

  /// Another mic, or the leveler stopped working: holds and counts start over (the list stays).
  pub fn switch_mic(&mut self) {
    (self.hold, self.recording) = (MicHold::default(), None);
  }

  fn on_event(&mut self, event: Event, chatmix: &mut ChatMixController, mic_id: Option<&str>, dir: &Path) {
    let pct = |v: f32| (v * 100.0).round();
    match event {
      Event::Moved { from, to, who, scene } => {
        let who = if who == Who::User { "user" } else { "app" };
        info!("[MicVol] Windows mic volume {:.0} % -> {:.0} %: {who} ({scene})", pct(from), pct(to));
      }
      Event::Held(Held { app: Some(app), listed: true, .. }) => {
        info!("[MicVol] {app} is on the held list: leveler held from the start");
      }
      Event::Held(Held { app: Some(app), .. }) => {
        let what = match note_volume_app(dir, &app) {
          1 => format!("first time for {app}: noted"),
          n => {
            self.listed.push(app.clone());
            if n == VOLUME_APP_LISTED {
              format!("second time for {app}: added to the held list")
            } else {
              format!("{n} times for {app}: on the held list")
            }
          }
        };
        info!("[MicVol] {app} moved the mic volume again: leveler held until it stops recording ({what})");
      }
      Event::Held(Held { apps, .. }) => {
        info!(
          "[MicVol] an app moved the mic volume again: leveler held until {} stop recording (several record, none noted)",
          apps.join(", ")
        );
      }
      Event::Released(held) => {
        let who = held.app.unwrap_or_else(|| held.apps.join(", "));
        let volume = match (held.restore, mic_id) {
          (Some(level), Some(id)) if (level - chatmix.mic.volume.min(1.0)).abs() > 0.015 => {
            chatmix.set_mic_level(id, level);
            format!("Windows mic volume back to {:.0} %", pct(level))
          }
          (Some(_), _) => "Windows mic volume as before".to_string(),
          (None, _) => format!("Windows mic volume stays at {:.0} %", pct(chatmix.mic.volume.min(1.0))),
        };
        info!("[MicVol] {who} stopped recording: leveler runs again, {volume}");
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const TICK: Duration = Duration::from_millis(250);

  /// One tick's world: recording apps, foreground, input `ago`, and a move from `from`.
  struct World {
    hold: MicHold,
    t0: Instant,
    n: u32,
    level: f32,
    recording: Vec<String>,
    listed: Vec<String>,
    front: Option<String>,
    input: Option<Duration>,
  }

  impl World {
    fn new(level: f32) -> Self {
      let hold = MicHold::default();
      let (t0, recording, listed, front, input) = (Instant::now(), vec![], vec![], Some("Game.exe".into()), None);
      Self { hold, t0, n: 0, level, recording, listed, front, input }
    }

    fn record(&mut self, apps: &[&str]) -> &mut Self {
      self.recording = apps.iter().map(|a| a.to_string()).collect();
      self
    }

    /// `secs` of ticks; `to` moves the volume on the first one.
    fn run(&mut self, secs: f32, to: Option<f32>) -> Vec<Event> {
      let mut events = vec![];
      for k in 0..(secs / 0.25).round().max(1.0) as u32 {
        let moved_from = to.filter(|_| k == 0).map(|to| std::mem::replace(&mut self.level, to));
        let tick = Tick {
          now: self.t0 + TICK * self.n,
          level: self.level,
          moved_from,
          recording: &self.recording,
          listed: &self.listed,
        };
        let (front, input) = (self.front.clone(), self.input);
        events.extend(self.hold.tick(&tick, || front, || input));
        self.n += 1;
      }
      events
    }

    fn held(&self) -> bool {
      self.hold.held().is_some()
    }
  }

  fn held_event(events: &[Event]) -> Option<&Held> {
    events.iter().find_map(|e| if let Event::Held(h) = e { Some(h) } else { None })
  }

  #[test]
  fn who_moved_it_by_the_window_in_front_and_what_records() {
    let teams = ["MSTeams".to_string()];
    let never = || -> Option<Duration> { panic!("input asked without the recording app in front") };
    assert_eq!(who(Some("Game.exe"), &[], never), Who::User, "nobody records");
    assert_eq!(who(Some("SystemSettings.exe"), &teams, never), Who::User);
    assert_eq!(who(Some("gamersequalizer.exe"), &teams, never), Who::User);
    assert_eq!(who(Some("Game.exe"), &teams, never), Who::App, "a game in front, typing or not");
    assert_eq!(who(None, &teams, never), Who::App);
    let recent = || Some(Duration::from_millis(400));
    assert_eq!(who(Some("ms-teams.exe"), &teams, recent), Who::User, "its own slider, by hand");
    assert_eq!(who(Some("ms-teams.exe"), &teams, || Some(Duration::from_secs(5))), Who::App);
    assert_eq!(who(Some("Zoom.exe"), &["zoom.exe".to_string()], recent), Who::User);
  }

  #[test]
  fn one_change_is_no_fight_a_second_within_a_minute_is() {
    let mut w = World::new(0.6);
    w.run(1.0, None);
    w.record(&["Zoom.exe"]);
    // Zoom sets the volume once at the start of the call: nothing happens.
    assert!(held_event(&w.run(70.0, Some(0.8))).is_none());
    // The next one comes after more than a minute: a new first one.
    assert!(held_event(&w.run(30.0, Some(0.9))).is_none());
    // And one 30 s later: held, Zoom the only one recording.
    let events = w.run(1.0, Some(1.0));
    let held = held_event(&events).unwrap();
    assert_eq!((held.app.as_deref(), held.restore, held.listed), (Some("Zoom.exe"), Some(0.6), false));
  }

  #[test]
  fn moves_less_than_2_s_apart_are_one_change_however_long() {
    let mut w = World::new(0.4);
    w.record(&["MSTeams"]);
    // An app ramping the volume in steps every 1.5 s for 15 s: one change, no fight.
    for k in 1..=10 {
      assert!(held_event(&w.run(1.5, Some(0.4 + 0.05 * k as f32))).is_none(), "step {k}");
    }
    let ended = w.run(3.0, None);
    assert!(ended.iter().any(|e| matches!(e, Event::Moved { who: Who::App, from, .. } if (from - 0.4).abs() < 1e-6)));
    assert!(!w.held());
    // A second change after the calm, still within the minute: held.
    assert!(held_event(&w.run(1.0, Some(0.7))).is_some());
  }

  #[test]
  fn the_hold_ends_when_its_app_stops_and_the_volume_goes_back() {
    let mut w = World::new(0.62);
    w.run(1.0, None);
    w.record(&["MSTeams"]);
    w.run(5.0, Some(0.7));
    assert!(held_event(&w.run(5.0, Some(0.8))).is_some());
    // Teams goes on raising it while held: the end goes back to before the call.
    w.run(5.0, Some(0.9));
    w.record(&[]);
    let events = w.run(1.0, None);
    let released = events.iter().find_map(|e| if let Event::Released(h) = e { Some(h) } else { None });
    assert_eq!(released.and_then(|h| h.restore), Some(0.62));
    assert!(!w.held());
  }

  #[test]
  fn the_users_own_change_during_the_call_stays() {
    let mut w = World::new(0.5);
    w.run(1.0, None);
    w.record(&["MSTeams"]);
    w.run(5.0, Some(0.6));
    w.run(5.0, Some(0.7));
    assert!(w.held());
    w.front = Some("SystemSettings.exe".into());
    w.run(5.0, Some(0.55));
    w.record(&[]);
    let events = w.run(1.0, None);
    assert!(events.iter().any(|e| matches!(e, Event::Released(Held { restore: None, .. }))), "{events:?}");
  }

  #[test]
  fn several_recording_apps_hold_until_all_stop_and_note_nobody() {
    let mut w = World::new(0.5);
    w.record(&["Discord.exe", "MSTeams"]);
    w.run(5.0, Some(0.6));
    let events = w.run(5.0, Some(0.7));
    assert_eq!(held_event(&events).map(|h| h.app.clone()), Some(None));
    w.record(&["Discord.exe"]);
    w.run(1.0, None);
    assert!(w.held(), "Discord still records");
    w.record(&[]);
    w.run(1.0, None);
    assert!(!w.held());
  }

  #[test]
  fn a_listed_app_is_held_from_its_start_also_next_to_another() {
    let mut w = World::new(0.5);
    w.listed = vec!["MSTeams".into()];
    w.record(&["OBS.exe"]);
    w.run(2.0, None);
    assert!(!w.held());
    w.record(&["MSTeams", "OBS.exe"]);
    let events = w.run(1.0, None);
    assert!(matches!(held_event(&events), Some(Held { app: Some(a), listed: true, .. }) if a == "MSTeams"));
    w.record(&["OBS.exe"]);
    w.run(1.0, None);
    assert!(!w.held(), "ends with the listed app");
  }

  #[test]
  fn changes_while_nobody_records_are_the_users_and_count_for_nothing() {
    let mut w = World::new(0.5);
    let events: Vec<Event> = [0.6, 0.7, 0.8].into_iter().flat_map(|to| w.run(5.0, Some(to))).collect();
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events.iter().all(|e| matches!(e, Event::Moved { who: Who::User, .. })), "{events:?}");
  }

  #[test]
  fn a_new_recording_counts_from_nothing() {
    let mut w = World::new(0.5);
    w.record(&["Zoom.exe"]);
    w.run(5.0, Some(0.6));
    w.record(&[]);
    w.run(1.0, None);
    w.record(&["Zoom.exe"]);
    assert!(held_event(&w.run(5.0, Some(0.7))).is_none(), "the first change of another call");
  }
}
