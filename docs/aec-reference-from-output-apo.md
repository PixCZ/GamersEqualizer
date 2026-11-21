# Brief: echo-canceller reference straight from the output APO

Working document for one implementation session. Remove it once the work is on `main`; move only the
measured results into `DEVLOG.md`.

Branch: create `feat/aec-reference-from-output-apo` from `main`. Read `AGENTS.md` and
`.claude/rules/architecture.md` first (real-time rules, shared-memory layout rules, no panics).

## Goal

The echo canceller (AEC) in the microphone APO needs to know what the speakers play ("the reference").
Today the reference travels the long way:

```
audiodg (output mix) -> WASAPI loopback -> service thread -> shared-memory ring -> mic APO (audiodg)
```

After this work the output APO, which already sees that signal inside audiodg, writes the ring itself:

```
output APO (audiodg) -> shared-memory ring -> mic APO (audiodg)
```

Decisions already made by the project owner. Do not reopen them:

- **No automatic fallback.** An output without our output effect gets no echo cancellation, and the app
  says so with the next step (install the effect there). The output effect is already required on that
  device for the EQ, game profiles and sidetone.
- **Every output sample rate must work** (44.1 to 192 kHz and beyond), not only 48 kHz.
- **The loopback is not deleted in this step.** It stays behind a hidden switch so both references can be
  compared on Windows. Deleting it is step 3, after the measurement, and only when the owner says so.

Why bother: the reference can be taken before the sidetone is mixed in (the user's own voice never
enters it; `DEVLOG.md` names this as the open risk of the loopback), two hops and one process boundary
go away, and the service loses a permanently open WASAPI stream.

## How it works today (verified in the code)

| Piece                          | Where                                                              | What it does                                                                                       |
| ------------------------------ | ------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| Ring and its time index        | `crates/gamersequalizer-common/src/shm/reference_ring.rs`          | 48 kHz stereo ring; `push_frames_timed(frames, qpc_first_hns)`; `ref_index_at(qpc)`; gaps = silence |
| Loopback capture               | `crates/gamersequalizer-daemon/src/win_audio/loopback.rs`          | Captures the Master output at 48 kHz float, pushes into the Local ring and, via `ApoLink`, the Global ring |
| Link to the APO                | `crates/gamersequalizer-daemon/src/win_audio/apo_link.rs`          | Created and ticked by the loopback thread; mirrors settings in, telemetry out; owns the delay cache |
| Reader                         | `crates/gamersequalizer-mic-chain/src/chain.rs`, `run_aec` (~l. 461) | Needs `tap_active`; `ref_index_at(mic time)`; delay estimator; reads stereo frames at `idx - offset` |
| Output APO                     | `crates/gamersequalizer-apo/src/output.rs`                         | EQ, then sidetone mix, then limiter. Knows its endpoint hash (`sidetone_endpoint`, set in `entry.rs` ~l. 280) |
| Block time                     | `crates/gamersequalizer-apo/src/clock.rs`, `entry.rs` `rt_apo_process` | Only `ApoKind::Mic` gets `qpc_hns`; `MicClock` smooths the raw times                              |
| Resampling                     | `crates/gamersequalizer-apo/src/rate_bridge.rs`                    | `Stage` = one rubato sinc resampler with a FIFO, mono, fed in 2 ms chunks                           |
| AEC status line                | `crates/gamersequalizer-daemon/src/ui/tab_mic.rs`, `aec_status_text` | `NoReference` shows "No speaker reference (loopback not running or no timestamps)"                |
| "Effect missing on the output" | `crates/gamersequalizer-daemon/src/health/mod.rs` (~l. 552, 299)   | `apo_install::check_flow(Flow::Render, ..)`, `Issue::OutputProcessing` with `Fix::OpenWindow`       |

Facts that shape the design:

- The ring has **one writer**. Nothing protects two writers from interleaving.
- The reference output is the Master device: the service loop publishes it in `win_audio/in_use.rs`,
  which the reference thread reads.
- `ApoLink::set_render_device` feeds the delay-cache key `(mic in use, render id)` and sets
  `reference_switch_pending`, which makes the APO measure the delay again. Today only `run_loopback`
  calls it.
- When nothing plays, Windows may not call the output APO at all. "No writer beat" therefore means
  "silent" as often as "effect missing"; the two must be told apart from the registry, not from the beat.
- `StereoFrame::from_channels` folds 5.1 / 7.1 down by channel count, not by `dwChannelMask` (its
  `shortcut:` comment says it is only reached when Windows refuses to convert the loopback). With this
  work it becomes the normal path for every multichannel output: update that comment.
- Nobody in the service or the window reads the Local ring (only writers were found). Confirm, then treat
  the Local ring write as dead code to delete in step 3.

## Step 1: the output APO writes the reference

### Shared memory (`crates/gamersequalizer-common/src/shm/`)

New header fields. Each goes into the struct, `init_defaults` and the right mirror list in `mirror.rs`;
bump `shm_version!` and the layout size constant (the build fails until both are done).

| Field                   | Written by          | Meaning                                                                                  |
| ----------------------- | ------------------- | ---------------------------------------------------------------------------------------- |
| `aec_ref_output_hash`   | service (control)   | `device_hash` of the reference output while a reference is wanted; 0 = nobody writes      |
| `aec_ref_writer_beat`   | output APO          | Bumped by the one instance that writes the ring (telemetry, mirrored out)                 |
| `aec_ref_status`        | service (Local)     | Why there is or is not a reference, for the window. Stored enum with a catch-all value    |

`tap_active` keeps its meaning for the reader ("a reference source exists"), but the service now sets
it from the registry: true when the reference output has our output effect installed (or when the
loopback is forced and capturing). That keeps `ReferenceSilent` ("nothing is playing") separate from
`NoReference` ("there is no source").

### Output APO (`crates/gamersequalizer-apo/src/`)

- `entry.rs`: pass a block time to `ApoKind::Output` too. `capture_time_hns` is written for capture
  (its fallback is "now minus the block"); a render block has not played yet, so its fallback is "now".
  Give the function a name that fits both, or add the render variant beside it.
- `clock.rs`: `MicClock` now serves both directions: rename it and its comments (names say what the
  thing is today). Raw call times jitter by milliseconds; unsmoothed, `push_frames_timed` would fill
  the gaps with silence.
- `rate_bridge.rs`: give `Stage` a channel count (the FIFO and scratch become interleaved, the two
  `InterleavedSlice` adapters take the count). `RateBridge` keeps using it with 1. Its tests must still
  pass unchanged.
- `output.rs`: a small reference feed owned by `OutputProcessor`:
  - Writes only when all hold: the owner is live, `aec_ref_output_hash != 0`, it equals this
    instance's endpoint hash, and the instance is the writer (see "One writer").
  - Tap point: after the EQ, **before** `add_sidetone`, in all three branches of `process_inner`
    (pass-through, full EQ, crossfade). The limiter after the tap only acts above 0 dBFS; accept that.
  - Per block: fold to stereo with `StereoFrame::from_channels`, resample to 48 kHz through a stereo
    `Stage` when the device rate is not 48 kHz (no `Stage` at 48 kHz), then
    `push_frames_timed(frames, time_of_first_frame)`.
  - Time of the first resampled frame: the block's smoothed time minus what was already queued in the
    stage FIFO, exactly as `RateBridge::process` computes `chain_time` (l. ~122).
  - Real-time rules: every buffer preallocated at build (`PREALLOC_FRAMES` as in the bridge), the
    resampler built in `create` (not in the audio thread), no logging per block. A resampler error
    sets a flag, stops the feed and leaves one `[ApoNote]`; the EQ keeps running.
  - `OutputProcessor::create` runs inside `catch_unwind` already; keep the feed inside it.
- Endpoint hash 0 ("the engine did not say", `entry.rs` l. 36): the instance cannot know whether it is
  the reference output and never writes. Leave one `[ApoNote]` so the app log explains it.

### One writer

Requirement: at most one instance writes the ring at any time, also when two output APO instances exist
for the same endpoint for a moment (re-initialization, the installer's `--probe-render`). First find
out from the `[ApoNote]` start/stop lines whether that can happen at all. A cheap guard that works
across processes: a writer id in the header (the instance id) taken by compare-exchange; another
instance takes over only after `aec_ref_writer_beat` stood still for a set time. Keep it that small;
no arbitration between sources is needed because the service decides the source (below).

### Service (`crates/gamersequalizer-daemon/src/win_audio/`)

- Source switch, hidden: environment variable `GAMERSEQUALIZER_AEC_REF=loopback`, read once at
  service start and logged. Default is the APO. Mark it with a `shortcut:` comment: it goes away with
  the loopback in step 3.
- Loopback thread (`loopback.rs` `spawn`), APO source: do not call `run_loopback`. Keep ticking
  `ApoLink`, and on every tick:
  - call `apo.set_render_device(<reference id>)` so the cache key and the re-measure on a switch keep
    working,
  - publish `aec_ref_output_hash` = hash of the reference id while `mic_dsp_live()`, else 0,
  - set `tap_active` and `aec_ref_status` from `apo_install::check_flow(Flow::Render, <reference id>)`.
    That call reads the registry: do it on a timer or on a device change, not every tick.
- Loopback source (forced): today's behaviour, with `aec_ref_output_hash` = 0 so no APO writes.
- `aec_delay_cache::key`: add the source, so a delay measured against the loopback never warm-starts
  the APO reference (the two differ by a few milliseconds). The echo-strength cache uses the same key.
- One log line per change, with the reason, for example
  `[AecRef] source APO on "Speakers (X)"` or `[AecRef] no reference: output effect not installed on "X"`.

### Window

- `aec_status_text` (`ui/tab_mic.rs`): replace the `NoReference` text by the reason from
  `aec_ref_status`: effect not installed on the named output (say that installing it fixes this),
  effect installed but not running, endpoint unknown, loopback forced. Texts go through `tr` / `trf`
  with the Czech entry in `i18n_cs.rs` in the same change. The value is read where `mic_status.rs`
  already reads the header, never in the paint path.
- Do not add a second health issue: check that `Issue::OutputProcessing` already fires when the mic
  chain is live and the reference output has no effect, and extend its second line only if it does not
  mention echo cancellation.

### Tests (run on Windows; the gate is `scripts/check.ps1`)

- `gamersequalizer-apo` `tests.rs`: a tone through `OutputProcessor` reaches the ring at the right
  time and level at 48, 44.1, 96 and 192 kHz and with 2, 6 and 8 channels; with sidetone on, the
  sidetone signal is not in the ring; nothing is written with a foreign hash, hash 0, a dead owner or
  `aec_ref_output_hash == 0`; block times with jitter leave no silent gaps in the ring.
- `rate_bridge.rs`: existing tests unchanged; one stereo `Stage` test (left and right stay apart).
- `shm/tests.rs`: defaults and mirror copies of the new fields; a mapping of the old version counts as
  missing.
- `aec_delay_cache`: keys of the two sources differ.
- The i18n test catches a missing Czech text by itself.

## Step 2: measurement on Windows (a human runs it)

Installing the APO needs admin and restarts Windows audio, so the owner does it. Then, on the same
device with the same music and speech, once with the default and once with
`GAMERSEQUALIZER_AEC_REF=loopback`:

| Check                                                    | Where to look                                             |
| -------------------------------------------------------- | --------------------------------------------------------- |
| Echo strength and fit                                    | `echo` and `fit` in the `[Chain]` lines of the app log    |
| Delay locks, and at what value                           | `[AEC] echo delay locked at ...`                          |
| Sidetone on, nothing playing, user speaks                | Reference level stays silent with the APO source          |
| Output at 192 kHz and at 44.1 kHz                        | Same checks; `[ApoNote]` shows no resampler error         |
| Output without the effect                                | Status line names the device and the fix                  |
| The reference itself                                     | `debug_capture` example, `02_ref_at_mic.wav`              |

Known risk to watch: if the delay never locks or sits at 0 ms, the render timestamp
(`u64QPCTime` of the V2 connection property) runs ahead of the real playback; then use "now" for the
output block time instead. Log once which time source the output APO uses.

Write the numbers into `DEVLOG.md` as a milestone (Measured / Derived from code / Open), condensed.

## Step 3: delete the loopback (only on the owner's go-ahead)

Loopback capture in `loopback.rs`, the Local ring write, `ApoLink::push_reference`, the environment
switch and the source part of the cache key's history. `ApoLink` needs a new driver: a plain supervised
worker thread. Update `README.md` (echo cancellation needs the output effect on the device) and the
process table in `.claude/rules/architecture.md` if its wording no longer fits.

## Not in this work

- Pausing the service while its user is not at the console (separate branch).
- Restart of the service after a crash, logging of the elevated probe under another account.
- Channel volumes inside the stream APO, a real Windows service, build-time changes.

## Working off Windows

The workspace builds and tests only on Windows. On macOS only clippy for the Windows target is
available (see the project memory note "Rust on the Mac"), plus `pwsh scripts/tests/apo-install.tests.ps1`.
Say plainly in the final report which of the tests above were written but not run.
