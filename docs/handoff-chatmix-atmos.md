# Handoff: Automatic ChatMix with Dolby Atmos (mix mode)

Written for: a developer or coding session that continues the work on branch `feat/chat-ducking`.
State of 2026-11-18, evening. Everything below comes from live tests on one PC (the user's headphones,
Dolby Atmos for Headphones always on) with a podcast in Brave as the "chat" (`brave.exe`) and The Division 2
(`thedivision2.exe`) as the game. Log: `%APPDATA%\GamersEqualizer\logs\gamersequalizer.log`, tag `[Ducking]`.

## 1. The problem and the design

Automatic ChatMix lowers the game while the chat speaks. With spatial sound (Atmos) a game that renders
spatial objects bypasses the per-app stream effects: its stream and its session meter play digital silence
(DEVLOG Milestone 19), so the stream APO cannot see or lower it. The user will not turn Atmos off.

| Mode   | Who lowers          | How                                                       | When                            |
| ------ | ------------------- | --------------------------------------------------------- | ------------------------------- |
| stream | the stream APO      | lowers the game's stream, full band                       | the game is heard at its stream |
| mix    | the service         | game level from the output mix, depth set as the game's session volume (full band) | the game "cannot be heard" |

- Mix ducker: `crates/gamersequalizer-common/src/ducking/mix.rs` (`MixDucker`); the decision is the stream
  ducker's `DuckGain` (`ducking/gain.rs`). Service side: `crates/gamersequalizer-daemon/src/win_audio/ducking/`
  (`mix.rs`, `unheard.rs`, `mod.rs`), session volumes in `win_audio/chatmix/game_duck.rs`
  (`ducking_lowered.json` = crash recovery of the lowered volumes).
- The game is measured in the mix, up and down, while the chat makes no sound at all (`chat_sounding` +
  guard `mix_chat_guard_s`). While the chat sounds, the mix less 4x the chat's own band level (its stream
  level at the output volume, peak-held 0.5 s) is a lower bound of the game and may only **raise** the held
  level, so a fight that starts in the middle of speech is heard in 0.5-4 s (`duck_sim`, podcast-like chat;
  before: 1-11 s, and never with the default guard 0.3 s because the pauses of fluent speech are shorter).
  Live log 2026-11-19 17:13: the first lowering came 5-10 s after the shooting started, because the
  game was measured only in pauses. This relies on the chat sitting in the mix at its stream's level (the log
  shows its peaks in the mix at its stream's peaks, or just under). The fall still waits for a pause.
- Mode switch: `UnheardWatch` says "cannot be heard" when a paired game stream plays and reads silence for
  3 s while spatial sound is on (was 10 s: the first sentences of a talk went unlowered, live log 2026-11-19
  16:59); ending it needs 10 s of audio (hysteresis, so the mode does not flap).
- The depth (both modes, since 2026-11-19 the two-level design, section 1a): 0 dB while nobody talks, the
  lowering level L while someone talks, reached at the slow/fast attack and left at the release rate.
- Tuning reaches the APO through shared memory slots (`duck_tune`, 48 slots, **append only**; the pinned
  list is in `ducking/tuning.rs` test `the_tuning_slots_are_pinned`). The user's file is
  `%APPDATA%\GamersEqualizer\duck_tuning.json`; no new APO needed for tuning changes.

## 1a. Two levels (2026-11-19, replaces the learned levels, lock and latch)

- Every run starts unlowered: L = 0, nothing is stored between runs (`duck_level_cache.json` and
  `level_store.rs` are gone; an old file is left on disk, unread).
- need = game - talk spurt + channel gains + margin (`strength * max_margin_db`). A spurt that is
  talking, at least `spurt_min_s` (0.2 s) old and needs more than `on_threshold_db` (2 dB) raises L to
  its need, bounded by `max_depth_db` (18 dB). L comes down only by `LEVEL_DECAY_DB_PER_S` (1 dB per 10 minutes, inaudible; the next
  fight raises it again by that little); a quieter fight keeps it.
- L starts over at 0 after 60 s without the game heard (band level plus game gain below -65 dB:
  `SILENT_RESET_S`, `SILENT_GAME_DB` in `gain.rs`, fixed for now), a stream gap, the feature off, a new
  strength, any Game / Chat channel gain change, and any Game / Chat session volume change (mix mode in
  `MixRun::follow_gains`; stream mode through `StreamSlot::duck_volume_epoch`, which the service bumps).
- A pause shorter than `hold_s` (3 s) stays in the spurt at the depth reached.
- Retired tuning slots (pinned at their old defaults, `retired_*`): `spurt_learn_s`, `loud_rise_s`,
  `loud_fall_db_per_min`, `quiet_fall_s`, `quiet_outlier_db`, `quiet_rise_db_per_min`, `loud_settled_s`,
  `quiet_settled_s`, `hysteresis_up_db`, `hysteresis_down_db`, `latch_release_s`. A value for them in
  `duck_tuning.json` is ignored.
- Log: the 10 s lines show game level, chat, spurts (and how many lowered), need, depth and
  `lowering level X dB (raised N)`; the minute summary ends with
  `lowering level X dB (raised N[, started over])`.
- Offline: the `--seed` option of `duck_sim` is gone; it reports the lowering level at the end and how
  many spurts raised it.

## 2. Commits and working tree

On `feat/chat-ducking`, not pushed (nothing of this branch is on `origin`):

- `8b5917b` stream-mode ducking (speech band, APO).
- `faa87a1` mix mode (service lowers the game session volume).
- `ed00a71` mix mode tuned by ear: steadier mode (hysteresis), minute label, `mix_hold_s` 3 s,
  `mix_max_margin_db` 17, `mix_max_depth_db` 18, lowering bar in the Automatic ChatMix row.

**Uncommitted** (made after `ed00a71`; the gate passed after each step except the last one, which had not
reported when this was written: run `scripts/dev-rebuild.ps1 -Test -Start` first):

| Change | File | Why |
| ------ | ---- | --- |
| `mix_chat_sound_db` = -45 dBFS (was a const -60 dBFS); `chat_sounding` takes it | `tuning.rs`, `mix.rs`, `win_audio/ducking/mix.rs`, `examples/duck_sim.rs` | the podcast never fell below -60 dBFS, so the game was never measured (section 4.1) |
| Pairing a silent game stream by elimination | `win_audio/stream_router.rs` | a game on Atmos could never be paired after a service restart (section 4.2) |
| `[StreamRouter] silent unpaired: ...` diagnostic line | `stream_router.rs` | the log said nothing about why a game was not paired |
| `mix_game_fall_s` = 6 s: the game level rises at once, falls slowly, counting the time since the last measurement | `tuning.rs`, `mix.rs`, tests | the game came back up at every quiet second of the game (section 4.4) |
| `mix_release_db_per_s` = 4 (was the stream's 12) | `tuning.rs`, `mix.rs`, `win_audio/ducking/mix.rs` | 13 dB came back in about 1 s and sounded like a jump |

Tests changed: `a_fight_starting_in_speech_is_picked_up_at_the_next_pause` (40 s of quiet instead of 10),
`a_game_going_calm_in_speech_is_followed_...` (several 0.4 s pauses; checks the release rate), new
`a_few_quiet_seconds_between_fights_do_not_let_go_of_the_duck_while_the_talk_goes_on`,
`chat_sounding_...` (threshold test at -50 / -60 dBFS), `pair_by_elimination` test in `stream_router.rs`.

## 3. What was observed (2026-11-18)

| Time | Observation |
| ---- | ----------- |
| 14:50-15:01 | Atmos on: game stream -120 dB the whole time, no ducking. Atmos off: game lowered 12-17 dB. (Original finding.) |
| 19:40-19:42 | Stream mode, Atmos switched on at 19:41:42 -> `cannot be heard` -> mix. Game lowered once, 3 dB for 6 s (target 13 dB, only 1 of 9 spurts latched). |
| 19:43-19:45 | Mix mode: game read -45 to -66 dB, chat peaks -31 dB, `need` negative: nothing lowered. |
| 19:52 | One lowering, 16.5 dB for 22 s, started right after a pause in the chat gave a measurement of the game (-40.5 dB). Looked as if tied to the GUI window being focused: it was not (the log shows a coincidence). |
| 19:56 | Last good run: EarTrumpet showed the game at 50 % going to 8 and back to 50 % after the podcast stopped. |
| 19:58 onward | After a service restart the game was never paired (section 4.2). |
| 20:17:56 | First `stream #2 -> TheDivision2.exe: silent, the only one` (elimination pairing works). |
| 20:19 | Game level read -70 dB with chat paired (game quiet at that moment); depth 0, correctly. Before the chat was paired the "game" read -40 dB: that was the podcast (section 4.3). |
| 20:31-20:33 | Louder spot: depth 13.1 dB when the game read -44 dB, but released again within 10 s buckets (pumping). |
| 20:39-20:44 (with `mix_game_fall_s`) | Depth held 13.4 dB for 181 s (20:40:17-20:43:08). Four releases, each when the game read -52 to -58 dB (against -40 dB in a fight) and `need` fell below 0. User: the return felt too fast (12 dB/s). |

| 20:47-20:52 (build with all uncommitted changes) | 20:47:56 game paired at once after the restart (`silent, the only silent Game session`), 20:48:06 mode mix, chat paired 20:48:27. Game read -70..-57 dB (quiet) with depth 0, correct. 20:50:38-20:51:31: one lowering of 11.9 dB held 53 s (game -44.6 dB, 100 % latched), no pumping, no warnings. Release at 20:51:34 (mean 6.1 dB in that 10 s window: ramp, not a jump). The 4 dB/s rate itself is not logged, so not measurable from the log. 20:51:36 chat "not paired" again with no logged cause; 20:52:42 game exited. Loud level -41.6 dB (stored) vs -63.0 dB in the minute summary: the 4.3 discrepancy is still there. |

## 4. Findings

### 4.1 The game was never measured with a continuous podcast
`chat_sounding` used -60 dBFS. A podcast has 66-115 "spurts" per minute and the chat sounded 67-87 % of the
time, so the game was almost never measured and its held level was stale (-62.0 dB identical for two
minutes). Raising the threshold to -45 dBFS lets breaths and tails count as pauses.
**Risk:** a threshold this high lets the chat's tail into the "game" reading. See 4.3.

### 4.2 After a service restart an Atmos game is never paired
`stream_router::evaluate` pairs a stream with a session only if the stream's envelope has audio to match
against the session meters (`streams::hold_match`). A game on Atmos plays silence at both, so after the router's
pairs are lost (service restart) it stays `StreamBus::None`, `UnheardWatch` never sees a game stream
playing, and the mode stays `stream` with "game not paired". It worked before only because the game had been
paired before Atmos was switched on. Fix: pair the silent unpaired stream(s) with the single silent unpaired
Game session, only after the same finding held for 6 evaluations (3 s) so a chat stream that is silent for a
moment before its first match is not taken. Observed: after one restart the router saw **2** silent streams
for 1 Game session (the game keeps more than one), hence "all silent streams to the one session".
Shortcut: a silent stream of another app is indistinguishable while the game is quiet; an audible match
corrects it.
Not covered: a game with no stream at all (never claimed a slot).

### 4.3 The "game" level can be the chat
While the chat stream is not paired (`chat_sounding` is never true) the whole mix is read as game. At 20:29:34,
when the chat was paired, the "game" fell from -40 dB to -70 dB. The learned "loud level" (-40.4 dB, "settled,
stored" in the 10 s lines) may therefore come from the podcast; the minute summary prints a different loud level
(-55 to -60 dB). Found 2026-11-19: with nothing lowered the summary took the game's silent stream
(first in the list) instead of the mix line; `view::main_game` now prefers the mix line at a tie.

### 4.4 Pumping
The depth is recomputed continuously from the game level, so a game that is quiet for a few seconds between
fights releases the duck while the talker goes on. `mix_game_fall_s` (6 s) damps it; the release rate was too
fast (12 dB/s) for a whole-game session volume, now `mix_release_db_per_s` 4.

### 4.5 Other observations
- The chat dropped to "not paired" repeatedly without a logged reason: 17:49:08, 20:01:09, 20:02:26, 20:09:20,
  20:13:12, 20:14:51, 20:43:42. Each time the ducker restarts. Cause unknown (the brave stream may be released
  when the podcast pauses; a new stream gets a new `stream #N`).
- The window size / focus of the game does not matter for detection (`[Game] foreground` is separate from stream
  pairing). A game running in the background is "not the active game" until it is foreground.
- After the first pairing, mix mode took 11 s to switch (10 s hold), earlier 73 s once; the reason for the
  73 s was not found.
- In stream mode each 10 s bucket is logged twice with different loud level / lock values (no explanation).
- The strength slider (50 % = 8.5 dB margin in mix mode, 100 % = 17 dB) replaced the plain 12 dB scale; the
  user's saved 80 % is now stronger than before (about 13.6 dB).

## 5. Open work, in order

0. **Done after this was written (uncommitted, Mac: common crate tests and clippy pass):** the mix
   ducker's loud level now learns while the game is lowered (the depth is undone) and each measurement
   counts for the time since the previous one, at most 2 s (`MAX_LOUD_STEP_S` in `ducking/mix.rs`;
   `DuckGain::update` takes `loud_dt` instead of `learn_loud`). Found offline: in a lobby scenario the
   loud level never learned a fight that started while ducked, so the depth stayed 2-6 dB. Dropped
   guard: a failed volume change or another app in the mix now inflates the loud level by up to the
   depth, bounded by `mix_max_depth_db`.
   Then (2026-11-19, see `docs/chatmix-redesign-comparison.md`): six tunables retired (learning
   slowdowns, warm-up values; slots kept as `retired_*`, so the installed APO is unaffected until
   reinstalled); mix mode raises the held game level only when two segments agree (one shot no longer
   lowers a calm game); mix mode shifts learned levels when the Game / Chat session volume moves and
   stores them as at 100 % (key `<exe> [mix@100]`; old `[mix]` records are not read). After review
   the previous segment ages over talk. A cap on learning while lowered was added and removed again:
   the live log of 2026-11-19 (game at ~41 % session volume) showed a fight growing under the
   reduction and never learned (depth 10 dB). Known cost without it: another app in the mix can
   carry the depth to 18 dB while it plays.
   Needs: the gate on Windows, an APO reinstall (gain.rs changed), a live run.

1. **Verify the uncommitted work**: gate, then a live run (loud game, continuous podcast). In the log expect
   `stream #N -> TheDivision2.exe ... silent`, `mode mix`, depth 13 dB held, releases only after the talk
   ends and 4 dB/s ramps.
2. **Done 2026-11-19:** `latch_release_s` 30 s instead of 3 s, for both modes now so a quiet game does
   not release the duck while someone talks; release after a pause in the speech (`mix_hold_s` 3 s). The
   user thinks the microphone auto-gain logic (`win_audio/mic_gain.rs`: steps only in a pause, dead band,
   cool-down) is the model: two states instead of continuous following.
3. **Lobby scenario** (the user asked): 20 minutes quiet game with constant talk, a fight, again 20 minutes.
   Risks: no pause in the speech freezes the held game level (first fight not ducked); the learned loud level
   may drift down during a long lobby. Simulate with `duck_sim --mix` on a long recording before another live
   test.
4. Check 4.3 (what the stored loud level is) and the chat un-pairing (4.5).
5. Not tested live: kill the app while lowered and check the volume is restored from `ducking_lowered.json`
   (`volume set back to ...`), restart the game while lowered, stream mode with Atmos off, other games and
   quieter / louder talkers.
6. Do not push before the user says so. No release notes are needed yet.

## 6. Tuning knobs in `duck_tuning.json` (mix ones)

| Name                   | Default | Meaning |
| ---------------------- | ------- | ------- |
| `mix_chat_guard_s`     | 0.3     | the game is measured this long after the chat last sounded |
| `mix_chat_sound_db`    | -45     | chat envelope peak above this counts as a sound |
| `mix_game_fall_s`      | 6.0     | the game level falls with this time constant (rises at once) |

Since 2026-11-19 both modes share one decision and its values: `max_depth_db` 18, `hold_s` 3.0,
`max_margin_db` 17 (50 % -> 8.5 dB), `release_db_per_s` 4.0, `on_threshold_db` 2. The old `mix_*` copies and
`latch_release_s` are retired slots (section 1a).

## 7. How to look at it

- Status line: `[Ducking] on, strength N %; mode mix|stream|none (...)`; "cannot be heard" / "can be heard
  again"; `silent unpaired: ...`; the 10 s `(mix)` lines (game level min/mean/max, chat, spurts, lowered,
  `need`, `depth`, `lowering level`, `mix level`, `chat sounding`); `last minute` summaries.
- Offline: `cargo run --release -p gamersequalizer-common --example duck_sim -- game.wav chat.wav out_dir
  [duck_tuning.json] [--strength 0.5] [--mix]`.
- Rules for the repo: `scripts/check.ps1` is the gate (run through `scripts/dev-rebuild.ps1 -Test -Start`),
  plain `cargo fmt` (never `--all`), UI texts through `tr()` with a Czech entry in `i18n_cs.rs`,
  new tuning names are appended to `PARAMS` and to the pinned list.
