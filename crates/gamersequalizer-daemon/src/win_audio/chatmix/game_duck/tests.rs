use super::*;

const GAME: &str = r"\Device\HarddiskVolume3\Games\Game\game.exe";
const OTHER: &str = r"\Device\HarddiskVolume3\Games\Other\other.exe";

/// A Game session as the mixer sees it: its volume, mute, and what was written to it.
#[derive(Clone)]
struct Fake {
  id: &'static str,
  path: &'static str,
  volume: f32,
  muted: bool,
  fail: bool,
  writes: Vec<f32>,
}

fn fake(id: &'static str, path: &'static str, volume: f32) -> Fake {
  Fake { id, path, volume, muted: false, fail: false, writes: Vec::new() }
}

/// One mixer pass as `apply_session_mix` runs it. `intent`: the value the mixer sets the Game
/// sessions to this pass (a full pass with a non-neutral or moved target): written by the mixer
/// itself to a session that is not lowered, handed over as the base for one that is.
fn pass(
  g: &mut GameDuck,
  now: Instant,
  sessions: &mut [Fake],
  subject: Option<&str>,
  factor: f32,
  intent: Option<f32>,
) {
  g.begin_pass(sessions.iter().map(|s| (s.id, s.path)));
  for s in sessions.iter_mut() {
    if let Some(i) = intent.filter(|_| !g.is_lowered(s.id)) {
      s.volume = i;
    }
    let look = Look { id: s.id, path: s.path, intent };
    let now_volume = (s.volume, s.muted);
    if let Some(p) = g.session(now, look, subject, factor, || Some(now_volume)) {
      if !s.fail {
        s.volume = p.volume;
        s.writes.push(p.volume);
      }
      g.done(look, p, !s.fail);
    }
  }
  g.end_pass(now, subject, sessions.iter().map(|s| (s.id, s.path)));
}

fn factor(db: f32) -> f32 {
  10f32.powf(-db / 20.0)
}

/// A reduction of `db` that holds for a few passes, then ends.
fn duck(g: &mut GameDuck, t: Instant, sessions: &mut [Fake], db: f32) {
  for _ in 0..3 {
    pass(g, t, sessions, Some(GAME), factor(db), None);
  }
  pass(g, t, sessions, Some(GAME), 1.0, None);
}

fn temp_dir(name: &str) -> PathBuf {
  let d = std::env::temp_dir().join(format!("geq_game_duck_{name}_{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&d);
  std::fs::create_dir_all(&d).unwrap();
  d
}

fn entry_json(path: &str, base: f32) -> String {
  format!(r#"{{ "{}": {{ "base": {base} }} }}"#, path.to_ascii_lowercase().replace('\\', "\\\\"))
}

#[test]
fn a_reduction_from_the_users_level_ends_exactly_there_every_time() {
  for start in [1.0, 0.60, 0.10] {
    let (mut g, t) = (GameDuck::default(), Instant::now());
    let mut s = [fake("s1", GAME, start)];
    duck(&mut g, t, &mut s, 6.0);
    assert_eq!(s[0].writes, [start * factor(6.0), start], "one write down, one exactly back");
    for _ in 0..10 {
      duck(&mut g, t, &mut s, 6.0);
    }
    assert_eq!(s[0].volume, start, "ten reductions later still exactly {start}");
  }
}

#[test]
fn a_ramp_writes_only_steps_of_0_2_db() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut s = [fake("s1", GAME, 0.6)];
  // 40 passes of 0.15 dB each: the first and then every second one is worth a write.
  for i in 1..=40 {
    pass(&mut g, t, &mut s, Some(GAME), factor(0.15 * i as f32), None);
  }
  assert_eq!(s[0].writes.len(), 20);
}

#[test]
fn the_fader_balance_and_mute_end_where_the_mixer_alone_would() {
  let t = Instant::now();
  // Our Game fader to 50 % mid-reduction: the session follows, and ends at the mixer's 50 %.
  let mut g = GameDuck::default();
  let mut s = [fake("s1", GAME, 1.0)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(0.5));
  assert_eq!(s[0].volume, 0.5 * factor(6.0));
  pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
  assert_eq!(s[0].volume, 0.5);
  // Mute on and off mid-reduction (the mixer sets 0, then its target 100 % on unmute).
  let mut g = GameDuck::default();
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(0.0));
  assert_eq!(s[0].volume, 0.0);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(1.0));
  assert_eq!(s[0].volume, factor(6.0));
  pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
  assert_eq!(s[0].volume, 1.0, "where the mixer alone ends after an unmute, not at 0");
  // Balance nudged away and back mid-reduction: the mixer's last value.
  let mut g = GameDuck::default();
  let mut s = [fake("s1", GAME, 1.0)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(0.8));
  assert_eq!(s[0].volume, 0.8 * factor(6.0));
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(1.0));
  assert_eq!(s[0].volume, factor(6.0));
  pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
  assert_eq!(s[0].volume, 1.0);
}

#[test]
fn a_muted_or_silent_session_is_never_written() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut muted = fake("s1", GAME, 0.6);
  muted.muted = true;
  let mut s = [muted, fake("s2", GAME, 0.0)];
  duck(&mut g, t, &mut s, 6.0);
  assert!(s.iter().all(|s| s.writes.is_empty()));
}

#[test]
fn another_programs_level_mid_reduction_is_overwritten_known_limit() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  s[0].volume = 0.3;
  pass(&mut g, t, &mut s, Some(GAME), factor(9.0), None);
  assert_eq!(s[0].volume, 0.6 * factor(9.0), "overwritten");
  pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
  assert_eq!(s[0].volume, 0.6);
}

#[test]
fn only_the_subjects_sessions_each_back_to_its_own_base() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut s = [fake("s1", GAME, 0.6), fake("s2", GAME, 0.4), fake("o1", OTHER, 0.7)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert_eq!((s[0].volume, s[1].volume), (0.6 * factor(6.0), 0.4 * factor(6.0)));
  // One of them ends mid-reduction: the other goes on and comes back on its own.
  let mut rest = [s[1].clone(), s[2].clone()];
  pass(&mut g, t, &mut rest, Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut rest, Some(GAME), 1.0, None);
  assert_eq!(rest[0].volume, 0.4);
  assert!(rest[1].writes.is_empty(), "the other game is never written");
}

#[test]
fn a_session_dragged_off_the_game_channel_gets_its_base_first() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  // The full pass finds it on Media now.
  let (path, planned) = g.left_game("s1").unwrap();
  assert_eq!(planned.volume, 0.6);
  g.done(Look { id: "s1", path: &path, intent: None }, planned, true);
  assert!(!g.is_lowered("s1"));
}

#[test]
fn losing_control_writes_every_base_at_once_and_a_clean_stop_leaves_no_entry() {
  let d = temp_dir("release");
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.6), fake("s2", GAME, 0.5)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert!(d.join(FILE).exists());
  let mut released = g.release();
  released.sort_by(|a, b| a.0.cmp(&b.0));
  let bases: Vec<(&str, f32)> = released.iter().map(|(id, _, p)| (id.as_str(), p.volume)).collect();
  assert_eq!(bases, [("s1", 0.6), ("s2", 0.5)]);
  for (id, path, p) in &released {
    g.done(Look { id, path, intent: None }, *p, true);
  }
  g.end_pass(t, None, s.iter().map(|s| (s.id, s.path)));
  assert!(!g.is_lowered("s1") && !g.is_lowered("s2"));
  assert!(!d.join(FILE).exists(), "nothing left to recover after a clean stop");
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failing_write_is_dropped_not_retried() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut bad = fake("s1", GAME, 0.6);
  bad.fail = true;
  let mut s = [bad];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert!(!g.is_lowered("s1") && g.failed.contains("s1"));
  let look = Look { id: "s1", path: GAME, intent: None };
  assert_eq!(g.session(t, look, Some(GAME), factor(6.0), || Some((0.6, false))), None, "no retry");
}

#[test]
fn a_failed_end_write_keeps_the_entry_for_the_next_session() {
  let d = temp_dir("failed_end");
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  s[0].fail = true;
  pass(&mut g, t, &mut s, None, 1.0, None); // the end write fails: still lowered for Windows
  pass(&mut g, t, &mut [], None, 1.0, None); // the game exits
  assert!(d.join(FILE).exists(), "kept: Windows keeps the lowered level");
  let mut later = [fake("s2", GAME, 0.6 * factor(6.0))];
  pass(&mut g, t, &mut later, None, 1.0, None);
  assert_eq!(later[0].writes, [0.6]);
  pass(&mut g, t, &mut [], None, 1.0, None);
  assert!(!d.join(FILE).exists());
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn recovery_sets_back_a_session_left_lowered_once_and_then_forgets() {
  let d = temp_dir("recovery");
  let file = d.join(FILE);
  let t = Instant::now();
  // At or above the base: the user's, untouched.
  for volume in [0.6, 0.8] {
    std::fs::write(&file, entry_json(GAME, 0.6)).unwrap();
    let mut g = GameDuck::for_service(&d);
    let mut s = [fake("s1", GAME, volume)];
    pass(&mut g, t, &mut s, None, 1.0, None);
    assert!(s[0].writes.is_empty());
  }
  // The fader was set to 50 % meanwhile: the mixer's value stays, no recovery over it.
  std::fs::write(&file, entry_json(GAME, 0.6)).unwrap();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.3)];
  pass(&mut g, t, &mut s, None, 1.0, Some(0.5));
  pass(&mut g, t, &mut s, None, 1.0, None);
  assert_eq!((s[0].volume, s[0].writes.len()), (0.5, 0));
  // The service was killed while the game kept running lowered (same session id).
  std::fs::write(&file, entry_json(GAME, 0.6)).unwrap();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.3)];
  pass(&mut g, t, &mut s, None, 1.0, None);
  pass(&mut g, t, &mut s, None, 1.0, None);
  assert_eq!(s[0].writes, [0.6], "set once");
  // A second session of the exe appears later, lowered too.
  let mut both = [s[0].clone(), fake("s2", GAME, 0.3)];
  pass(&mut g, t, &mut both, None, 1.0, None);
  assert_eq!(both[1].writes, [0.6]);
  assert!(file.exists(), "kept while its sessions play");
  pass(&mut g, t, &mut [], None, 1.0, None);
  assert!(!file.exists(), "gone once they were all checked and are over");
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_new_session_in_the_pass_its_lowered_one_vanished_is_recovered_first() {
  let d = temp_dir("same_pass");
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  // s1 vanishes; s2 appears in the same pass at the lowered level Windows kept.
  let mut s2 = [fake("s2", GAME, 0.6 * factor(6.0))];
  pass(&mut g, t, &mut s2, Some(GAME), factor(6.0), None);
  assert_eq!(s2[0].writes, [0.6], "set back first");
  pass(&mut g, t, &mut s2, Some(GAME), factor(6.0), None);
  assert_eq!(s2[0].writes, [0.6, 0.6 * factor(6.0)], "then lowered from 0.60");
  pass(&mut g, t, &mut s2, Some(GAME), 1.0, None);
  assert_eq!(s2[0].volume, 0.6);
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_non_ascii_path_keeps_its_entry_for_the_whole_run() {
  const HRA: &str = r"D:\Игры\Šachy\hra.exe";
  let d = temp_dir("non_ascii");
  let file = d.join(FILE);
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", HRA, 0.6)];
  pass(&mut g, t, &mut s, Some(HRA), factor(6.0), None);
  assert!(file.exists(), "on disk during the reduction");
  let marked = std::fs::read_to_string(&file).unwrap().replacen("\"base\"", "\"mark\": 1, \"base\"", 1);
  std::fs::write(&file, &marked).unwrap();
  for _ in 0..5 {
    pass(&mut g, t, &mut s, Some(HRA), factor(6.0), None);
    pass(&mut g, t, &mut s, Some(HRA), 1.0, None);
  }
  assert!(std::fs::read_to_string(&file).unwrap().contains("mark"), "not rewritten per spurt");
  pass(&mut g, t, &mut s, None, 1.0, None);
  assert!(!file.exists());
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn two_sessions_of_one_game_keep_the_louder_base_and_write_once() {
  let d = temp_dir("two");
  let file = d.join(FILE);
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.6), fake("s2", GAME, 0.4)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert!(std::fs::read_to_string(&file).unwrap().contains("0.6"));
  let marked = std::fs::read_to_string(&file).unwrap().replacen("\"base\"", "\"mark\": 1, \"base\"", 1);
  std::fs::write(&file, &marked).unwrap();
  for _ in 0..300 {
    pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
    pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  }
  assert!(std::fs::read_to_string(&file).unwrap().contains("mark"), "written once in 300 spurts");
  pass(&mut g, t, &mut s, None, 1.0, None);
  assert_eq!((s[0].volume, s[1].volume), (0.6, 0.4));
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_file_follows_runs_not_spurts() {
  let d = temp_dir("file");
  let file = d.join(FILE);
  let t = Instant::now();
  let mut g = GameDuck::for_service(&d);
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert!(file.exists(), "added before the first lowering write");
  // Mark the file on disk: a rewrite would drop the mark.
  let marked = std::fs::read_to_string(&file).unwrap().replacen("\"base\"", "\"mark\": 1, \"base\"", 1);
  std::fs::write(&file, &marked).unwrap();
  pass(&mut g, t, &mut s, Some(GAME), 1.0, None);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  assert!(std::fs::read_to_string(&file).unwrap().contains("mark"), "later spurts do not rewrite it");
  // A base that moves is written at most every 5 s.
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(0.5));
  assert!(std::fs::read_to_string(&file).unwrap().contains("mark"));
  pass(&mut g, t + BASE_SAVE_EVERY, &mut s, Some(GAME), factor(6.0), None);
  assert!(std::fs::read_to_string(&file).unwrap().contains("0.5"));
  // The run ends with the session back: the entry goes.
  pass(&mut g, t + BASE_SAVE_EVERY, &mut s, None, 1.0, None);
  assert!(!file.exists());
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_file_keeps_what_it_does_not_know_and_survives_bad_states() {
  let t = Instant::now();
  // Unknown fields and unreadable entries survive a save.
  let d = temp_dir("unknown");
  let file = d.join(FILE);
  std::fs::write(&file, r#"{ "x:\\gone.exe": "garbage", "y:\\kept.exe": { "base": 0.5, "note": 1 } }"#).unwrap();
  let mut g = GameDuck::for_service(&d);
  pass(&mut g, t, &mut [fake("s1", GAME, 0.6)], Some(GAME), factor(6.0), None);
  let text = std::fs::read_to_string(&file).unwrap();
  assert!(text.contains("garbage") && text.contains("note"), "{text}");
  // A corrupt file is kept aside; the store starts empty.
  std::fs::write(&file, "{ nope").unwrap();
  let _ = GameDuck::for_service(&d);
  assert!(d.join(format!("{FILE}.corrupt")).exists());
  let _ = std::fs::remove_dir_all(&d);
  // A file that cannot be read (not UTF-8) is never overwritten, although a write would work.
  let d = temp_dir("unreadable");
  let file = d.join(FILE);
  let bytes = [0xff, 0xfe, 0x00, 0x7b];
  std::fs::write(&file, bytes).unwrap();
  let mut g = GameDuck::for_service(&d);
  pass(&mut g, t, &mut [fake("s1", GAME, 0.6)], Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut [], None, 1.0, None);
  assert_eq!(std::fs::read(&file).unwrap(), bytes);
  let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn nothing_is_ever_louder_than_the_base() {
  let (mut g, t) = (GameDuck::default(), Instant::now());
  let mut s = [fake("s1", GAME, 0.6)];
  pass(&mut g, t, &mut s, Some(GAME), f32::NAN, None);
  assert!(s[0].writes.is_empty(), "a bad depth is no reduction");
  pass(&mut g, t, &mut s, Some(GAME), -1.0, None);
  assert_eq!(s[0].volume, 0.0, "clamped to 0, not negative");
  pass(&mut g, t, &mut s, Some(GAME), 2.0, None);
  assert_eq!(s[0].volume, 0.6, "a factor over 1 ends the reduction at the base");
  // A mixer value over 1 (never sent; bad data) is clamped: the base is at most 1.
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), None);
  pass(&mut g, t, &mut s, Some(GAME), factor(6.0), Some(3.0));
  assert_eq!(s[0].volume, factor(6.0));
}
