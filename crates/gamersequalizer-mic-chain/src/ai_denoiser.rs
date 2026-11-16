//! Neural noise suppression: RNNoise (nnnoiseless, light), DeepFilterNet 3 (higher quality,
//! ~2 % of a core) or DPDFNet-2 (best on soft speech, ~11 %, see `dpdfnet`). The heavy models
//! load on first selection (DFN3 ~80 ms, DPDFNet ~120 ms) and then stay in memory. The stream
//! loads its selected model before audio starts (`set_model`); a live switch (`request_model`)
//! loads on a short-lived thread while RNNoise keeps running, so the audio thread never stalls.
//! The raw-input voice detector (`in_net`) runs RNNoise 0.2 (`rnnoise2`), everything else the
//! 2017 RNNoise network.

use crate::dpdfnet::{self, DpdfEngine};
use crate::frame_fifo::{FrameInput, FrameOutput, I16_SCALE};
use df::tract::{DfParams, DfTract, RuntimeParams};
use gamersequalizer_common::shm::{DenoiseLimit, DenoiseModel};
use ndarray::Array2;
use nnnoiseless::{DenoiseState, FRAME_SIZE};
use rnnoise2::DenoiseState as DenoiseState2;
use silero_vad::Vad48k;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

/// Attenuation limit of the heavy models at strength 0+ .. just below 1.0 (100 % = unlimited).
const DF_MIN_ATTEN_DB: f32 = 6.0;
const DF_MAX_ATTEN_DB: f32 = 60.0;
/// Below this local SNR DeepFilterNet treats a frame as noise only and zeroes it. Upstream's
/// -10 dB dropped 16-26 % of soft syllables under loud mechanical keys; -20 dB drops 3-8 %
/// with the same key suppression (`test_deepfilter_keeps_soft_speech_over_keyboard`). The model's
/// LSNR output never goes below -15 dB (`lsnr_min` in its config), so -20 turns zeroing off.
const DF_MIN_DB_THRESH: f32 = -20.0;
/// Above this local SNR DeepFilterNet leaves a frame untouched. Upstream's 30 dB let its ERB
/// mask duck clean unvoiced speech (s, f, breath) by 10-15 dB in frames it rated 20-29 dB:
/// on a real mic take 20 % of loud unvoiced frames lost > 6 dB, at 25 dB 10 % (DPDFNet 11 %),
/// for ~2 dB less suppression in a quiet room's pauses (the gate covers those).
const DF_MAX_DB_ERB_THRESH: f32 = 25.0;
/// Upstream default: above this only the ERB stage runs (no deep filtering).
const DF_MAX_DB_DF_THRESH: f32 = 20.0;
/// A frame is speech above this RNNoise voice probability. Measured on mechanical keys + mouse
/// clicks without speech: never fired at 0.5 (DeepFilterNet's local SNR fired on 9-31 %).
const VOICE_PROB: f32 = 0.5;
/// ...for this many 10 ms frames in a row: a key click is shorter than a syllable, and very
/// loud keys (-20 dBFS) occasionally score one speech-like frame.
const VOICE_MIN_FRAMES: u32 = 2;
/// Once speech was detected, frames above this much weaker probability keep the gate open
/// (soft syllables, word tails), as long as speech was heard within `VOICE_ACTIVE_FRAMES`.
/// Measured (`gate_sweep`): soft syllables cut by the gate under loud keys 5.3 -> 0.6 % (RNNoise),
/// 3.5 -> 0.6 % over no gate (DFN3); keys alone score above it on 2-8 % of frames, a fan on
/// 0.4-1.6 %, and typing right after speech keeps the gate open +0.2 % longer.
const VOICE_KEEP_PROB: f32 = 0.05;
const VOICE_ACTIVE_FRAMES: u32 = 20;
/// `in_net` runs RNNoise 0.2 (`rnnoise2`, Full model), so `vad` opens and keeps at these instead
/// while `in_net` feeds it (`VoiceNet::thresholds`; `VOICE_MIN_FRAMES`/`VOICE_ACTIVE_FRAMES`
/// unchanged). Measured (rnnoise2 `vad_sweep`: speech -20 dBFS under ESC-50 keys at 0 and
/// -10 dB SNR, keys alone at -20 dBFS): keys alone score above 0.7 / 0.2 on 0.5 / 5.2 % of
/// frames (2017 model at 0.5 / 0.05: 0.4 / 7.8 %), while speech frames are heard 94-97 % (old
/// 76-82 %) and quiet ones (clean RMS -40..-15 dB, syllable onsets and tails) 31-38 / 56-59 %
/// (old 10-12 / 30-48 %). Same false-alarm rate, far fewer cut syllables.
const IN_NET_VOICE_PROB: f32 = 0.7;
const IN_NET_KEEP_PROB: f32 = 0.2;
/// The second detector, on the denoised output, opens on one frame above this and stays open
/// longer: keys are mostly gone there, and a cut voice is worse than a few keys let through.
/// Measured on a private voice under its own keyboard (`private_report`): phrase onsets after
/// a pause scored 0.6-0.9 there for one frame but < 0.1 on the raw input; soft syllables cut
/// by the gate over no gate 6-15 -> 1-4 %. Cost: typing alone (ESC-50 keys, -20 dBFS) keeps
/// it open most of the time with RNNoise/DFN3, so only the floor below stops those keys.
/// Silero VAD on the raw input (`soft_net`) opens the gate above this, next to the detector
/// above. It never reaches it on keys or mouse clicks alone (highest 0.08 on the ESC-50 keys)
/// and hears 70-95 % of whispered frames, where `in_net` at its keep threshold hears 30-62 %
/// (DEVLOG milestone 38). In the chain (`private_report`, gate auto + lookahead, private voice
/// under private keys): soft syllables the gate cut 4.0 -> 2.4 % (DFN3, voice -45 dBFS; 2.4 %
/// is the model alone) and 2.1-2.4 -> 0.3 % (RNNoise), for 0.1-2.2 dB less key suppression in the
/// short pauses between words; the gate on keys alone opens exactly as often as before. At 0.5:
/// 3.7 % and 1.2-2.3 %. `whisper_gate_report`: whispered frames under a closed gate 40 -> 2 %.
const SOFT_VOICE_PROB: f32 = 0.15;
const OUT_VOICE_PROB: f32 = 0.3;
const OUT_VOICE_MIN_FRAMES: u32 = 1;
const OUT_VOICE_KEEP_PROB: f32 = 0.02;
const OUT_VOICE_ACTIVE_FRAMES: u32 = 50;
/// A closed gate attenuates by this much instead of muting, so a late open only makes a word
/// onset quieter, never cuts it (lost soft syllables, own voice: up to 1.6 percentage points
/// less than muting). Keys between words come through 12 dB under the denoiser's output.
const GATE_FLOOR_DB: f32 = -12.0;
/// The floor goes up to this much further down while the leveler boosts (`set_gate_floor_extra_db`),
/// so the noise in pauses doesn't rise with the leveler's gain.
const GATE_FLOOR_EXTRA_MAX_DB: f32 = 12.0;
/// Most the input may be raised for DeepFilterNet (see `AiVoiceIsolator::set_pre_gain_db`).
const DF_PRE_GAIN_MAX_DB: f32 = 24.0;
/// Shared-memory gate value at or below this = gate off; anything above = automatic gate.
/// (The setting used to be a level threshold; the gate no longer has one.)
const GATE_OFF_DB: f32 = -79.5;
/// Gate value at or above this = automatic gate with lookahead (`NOISE_GATE_AUTO_LOOKAHEAD`).
const GATE_LOOKAHEAD_DB: f32 = 0.5;
/// Lookahead: the output is delayed this long, so the gate opens before a word onset reaches it
/// (the detectors still hear it undelayed). Per model (`lookahead_samples`): the input detector
/// is already a model latency ahead of the output, so heavier models need less of it.
/// History, 30 ms for all (private voice under its own keyboard, `private_report`): soft
/// syllables the gate cuts over no gate, DFN3 1.4-3.7 -> 0.3-2.3 %, DPDFNet 0.8-1.3 -> 0.2-0.9 %,
/// RNNoise 1.9-3.5 -> 1.4-1.9 %; 50 ms only ~0.4 points better. Keys alone open the gate no
/// more often. Per model (`lookahead_sweep`, private voice, 2026-11-08; soft syllables lost in
/// the three mixes, keys open unchanged): 15 ms costs RNNoise 0.2-0.5 points over 30 ms
/// (20 ms is no better than 15), DFN3 0.2-0.5, and DPDFNet loses nothing down to 10 ms (its
/// 60 ms latency already puts the input detector far ahead). Together with AEC3's ~9 ms
/// every model ends up below its old 30 ms total.
const RNNOISE_LOOKAHEAD_SAMPLES: usize = 48 * 15;
const DF_LOOKAHEAD_SAMPLES: usize = 48 * 15;
const DPDF_LOOKAHEAD_SAMPLES: usize = 48 * 10;
/// Gate stays open this long after the last speech frame (word tails, short pauses), counted
/// at the output: the detector hears the raw input a model latency early, so the gate adds
/// that latency on top (else DPDFNet's 60 ms lead would leave only ~140 ms).
const GATE_HOLD_SAMPLES: usize = 48 * 200;
/// No speech on the input detector for this long puts a heavy model to sleep (see `asleep`).
const SLEEP_AFTER_SAMPLES: usize = 48_000 * 2;
/// Crossfade once a takeover's newcomer has filled its latency (5 ms).
const TAKEOVER_FADE_SAMPLES: usize = 240;
/// RNNoise runs this long next to the heavy model before it stands in: its state is from the
/// last pause, and stale it rated the room as speech and woke the model right away.
const STAND_IN_WARMUP_SAMPLES: usize = 48 * 300;
/// Input this far over the room's noise floor counts as activity: it keeps the heavy model
/// awake / wakes it. The voice detector misses whisper and a quiet voice (mic set low), which
/// RNNoise standing in then lost by 14-20 dB (`leveler_report`); sleeping only in a quiet room
/// (stationary noise, where RNNoise does as well) keeps them.
const ACTIVITY_OVER_FLOOR_DB: f32 = 12.0;
/// Activity must last this long (voice does; a single click does not).
const ACTIVITY_MIN_SAMPLES: usize = 48 * 20;
/// The level compared with the floor is smoothed over ~100 ms: single 10 ms blocks of a quiet
/// room jump 12 dB over its quietest block several times a second and kept the model awake.
/// The floor itself stays the quietest raw block: a smoothed floor rose to steady typing and to
/// 4 s of quiet speech, and the model slept through both.
const ACTIVITY_SMOOTH_SAMPLES: f32 = 48.0 * 100.0;
/// Noise floor = the quietest block of the last `FLOOR_SEGMENTS` x 100 ms (minimum statistics:
/// recovers within 1.5 s after a quiet moment; a rising floor at 2 dB/s took 11 s and kept the
/// model awake in a quiet room).
const FLOOR_SEGMENTS: usize = 15;
const FLOOR_SEGMENT_SAMPLES: usize = 4_800;
const FLOOR_MIN_DB: f32 = -100.0;
/// Per-sample smoothing of the gate gain: ~1 ms to open, ~50 ms to close (no clicks).
const GATE_OPEN_COEF: f32 = 1.0 / 48.0;
const GATE_CLOSE_COEF: f32 = 1.0 / (48.0 * 50.0);
/// Click rescue (DeepFilterNet only, see `ClickRescue`): DFN3 output this far under RNNoise's
/// output of the same moment, while speech was heard, counts as a voice DFN3 cut.
const RESCUE_RATIO_DB: f32 = 6.0;
/// Speech heard on DFN3's own output arms the rescue this long (+ the 30 ms crossfade out); keys
/// within it come through next to words. Measured with an earlier raw-input arming (`click_report`,
/// private voice, clicks at -3 dBFS peak): frames lost > 15 dB 3.4 % at 50 ms, 3.1 % at 100,
/// 2.4 % at 200 ms, but 200 ms let 14 dB more clicks into short pauses.
const RESCUE_HOLD_SAMPLES: usize = 48 * 100;
/// Voice probability on DFN3's output that arms it (0.3 / 0.5 / 0.7 measured alike).
const RESCUE_VOICE_PROB: f32 = 0.5;
/// The rescue fills in only while Silero (`soft_net`) hears a voice in the raw input: one of its
/// last two chunks over this. Live capture 2026-11-09 20:49 (quiet voice, mouse hammered): a
/// click 300 ms after a word came out at -45.9 dBFS, louder than the voice, because DFN3 left
/// enough of it for the rescue's own detector to arm on; Silero read 0.03 there. With this
/// (`capture_replay_report`): -80.2 dBFS, every other 100 ms window within 6 dB of the live
/// output, voice windows unchanged (mean loss 2.6 dB). 0.3 also drops a second leak (51.5 s) and
/// costs 1 point of word tails on the private keys mix; 0.5 starts to cost voice in the capture.
/// Synthetic mixes at 0.15: frames lost > 15 dB and under hammered clicks unchanged (3.4 %,
/// 5.8 %), speech frames lost > 6 dB 7.5 -> 7.8 % (`near_pause_report`).
const RESCUE_SOFT_PROB: f32 = 0.15;
/// Energy envelopes (~5 ms); crossfade in (~2 ms), out (~30 ms).
const RESCUE_ENV_COEF: f32 = 1.0 / (48.0 * 5.0);
const RESCUE_IN_COEF: f32 = 1.0 / (48.0 * 2.0);
const RESCUE_OUT_COEF: f32 = 1.0 / (48.0 * 30.0);
/// Added latency, 48 kHz samples. Both = output FIFO priming (one frame, so any block size
/// works) + the model's own delay. Measured in `test_measured_latency_matches_constants`.
/// RNNoise: 480 priming + 480 overlap-add window.
const RNNOISE_LATENCY_SAMPLES: usize = 2 * FRAME_SIZE;
/// Silent frames that push old audio out of RNNoise on a flush: its pitch buffer (1728 samples)
/// needs 4, its input high-pass (pole 0.998, ~500-sample time constant) 10 to fade under -80 dB.
/// About 0.2 ms of CPU, once per pause.
const RNNOISE_FLUSH_FRAMES: usize = 10;
/// DeepFilterNet 3: 480 priming + (fft 960 - hop 480) STFT overlap + 2 lookahead frames * 480.
const DF_LATENCY_SAMPLES: usize = 4 * 480;
/// DPDFNet: 480 priming + 480 STFT overlap + 4 lookahead hops (2880 = 60 ms).
const DPDF_LATENCY_SAMPLES: usize = dpdfnet::LATENCY_SAMPLES;

/// Internal state for the debug capture (`AiVoiceIsolator::debug_state`), after the last block.
/// Probabilities are the highest of the frames completed in the block, -1 when none completed.
#[derive(Clone, Copy, Default)]
pub struct DenoiseDebug {
  pub in_prob: f32,
  pub out_prob: f32,
  pub rescue_prob: f32,
  /// Silero VAD on the raw input (the gate's second key), -1 when no chunk completed.
  pub soft_prob: f32,
  pub vad_run: u32,
  pub vad_active_left: u32,
  pub vad_out_active_left: u32,
  pub gate_enabled: bool,
  /// Linear gate gain and floor (incl. the leveler extra).
  pub gate_gain: f32,
  pub gate_hold_left: usize,
  pub gate_floor: f32,
  /// Linear DeepFilterNet pre-gain (1.0 = none).
  pub df_pre_gain: f32,
  pub rescue_mix: f32,
  pub rescue_hold_left: usize,
  pub bypassed: bool,
  pub quiet_samples: usize,
  pub takeover: bool,
}

pub struct AiVoiceIsolator {
  rnnoise: RnNoiseEngine,
  deepfilter: Option<Box<DeepFilterEngine>>,
  df_error: Option<String>,
  dpdfnet: Option<Box<DpdfEngine>>,
  dpdf_error: Option<String>,
  /// The selected heavy model was too slow for real time on this CPU; RNNoise runs until a
  /// model is selected again.
  overloaded: bool,
  selected: DenoiseModel,
  active: DenoiseModel,
  strength: f32, // 0.0 to 1.0
  /// Linear gain DeepFilterNet gets on its input and gives back on its output (1.0 = none).
  df_pre_gain: f32,
  gate: NoiseGate,
  /// Speech on the raw input (the leveler; the gate, a model latency ahead of the output). Fed by
  /// the RNNoise denoiser's own voice probability while it runs, else by `in_net`.
  vad: VoiceHysteresis,
  /// Same detector on the denoised output: keys are gone there, so it keeps hearing a voice the
  /// raw-input one loses under typing (private voice, keys at -10 dB SNR: RNNoise's probability
  /// on soft syllables fell from ~0.95 to <0.1 and the gate cut 18-31 % of them).
  vad_out: VoiceHysteresis,
  /// RNNoise run only for its voice probability: on the raw input while no RNNoise denoises it
  /// (RNNoise 0.2, Full; ~10x the 2017 pass, ~2.8 % of a core). Its probability describes the
  /// frame before the one just fed (one frame of network lookahead), so it reports 10 ms later
  /// than the 2017 detector did; it still leads the heavy model's output by that model's
  /// latency - 10 ms (DFN3 30 ms, DPDFNet 50 ms) plus the lookahead, so the output latency does
  /// not change and the gate's hold (counted from the report) only ends 10 ms later...
  in_net: VoiceNet,
  /// ...and on the denoised output, with the gate on.
  out_net: VoiceNet,
  /// Silero VAD on the raw input, with the gate on: a second key for the gate only. The leveler,
  /// the automatic input gain and the model's sleep keep to `vad` (`heard_voice`): a whisper it
  /// hears must not be learned as the speech level. One probability per 32 ms, still ahead of
  /// the output by the model's latency + lookahead - 32 ms (RNNoise alone: 3 ms). None: its
  /// weights failed to load (a corrupt build), the gate runs without it.
  soft_net: Option<Vad48k>,
  /// `SOFT_VOICE_PROB`; tests switch the key off with infinity.
  soft_voice_prob: f32,
  /// Its last two probabilities (older first).
  soft_recent: [f32; 2],
  /// Lookahead requested by the settings; it runs only while the gate is on.
  lookahead: bool,
  /// Tests only: lookahead length instead of the model's (`lookahead_sweep`).
  #[cfg(test)]
  lookahead_override: Option<usize>,
  /// Tests only: DFN3 without `ClickRescue`, for comparison.
  #[cfg(test)]
  no_rescue: bool,
  delay: DelayLine,
  bypassed: bool,
  /// The input detector heard speech in the last block (it runs while bypassed too).
  heard_voice: bool,
  sleep: Sleep,
  /// DeepFilterNet only: RNNoise runs next to it and fills in where DFN3 cut a voice (clicks).
  rescue: ClickRescue,
  /// Highest voice probability of the raw-input detector / `out_net` / the rescue's detector in
  /// the last block, -1 when none ran (debug capture only; a compare per 10 ms frame).
  in_prob: f32,
  out_prob: f32,
  rescue_prob: f32,
  soft_prob: f32,
  /// Quietest block per 100 ms segment, the last `FLOOR_SEGMENTS` (dBFS; +inf = no data yet).
  floor_segments: [f32; FLOOR_SEGMENTS],
  floor_pos: usize,
  floor_segment_samples: usize,
  /// Input power smoothed over `ACTIVITY_SMOOTH_SAMPLES`.
  level_power: f32,
  /// Samples in a row over the floor + `ACTIVITY_OVER_FLOOR_DB`.
  active_run: usize,
  sleep_after: usize,
  /// The second path's copy of the block during a handover.
  spare: Vec<f32>,
  /// Heavy model being loaded on the loader thread (one at a time).
  loading: Option<(DenoiseModel, Receiver<HandOver>)>,
  /// Heavy-model output samples still to silence after a `flush`: its recurrent state and
  /// lookahead frames hold the audio from before (one model latency of it).
  mute_left: usize,
}

/// The heavy model's sleep: what a pause, a model switch and a reset all start over (`clear`).
/// The room's noise floor (`note_activity`) is not part of it: a pause doesn't change the room.
struct Sleep {
  /// Heavy model asleep: no speech for `sleep_after` samples, so RNNoise (delayed to the heavy
  /// model's latency) covers the pause and the heavy model stops (DFN3 ~3 %, DPDFNet ~10 % of a
  /// core). Speech on RNNoise's own detector wakes it.
  asleep: bool,
  /// Both run while one takes over from the other (see `Takeover`).
  takeover: Option<Takeover>,
  /// Input samples since the input detector last heard speech or saw activity.
  quiet_samples: usize,
  /// RNNoise output delayed to the heavy model's latency while it stands in.
  delay: DelayLine,
}

/// The heavy model and the RNNoise stand-in hand over without a gap: from the start both get
/// the input; the output stays on the old one until the newcomer's first real sample comes out
/// (both paths have the heavy model's latency, so they are aligned), then crossfades. The input
/// detector keeps its own pass until the heavy model is off.
#[derive(Clone, Copy)]
struct Takeover {
  to_sleep: bool,
  /// Output samples since the newcomer started.
  done: usize,
  /// Output sample the crossfade starts at: the newcomer's latency (and RNNoise's warm-up).
  fade_at: usize,
}

/// A heavy engine, or why it could not be loaded.
enum Loaded {
  DeepFilter(Result<Box<DeepFilterEngine>, String>),
  Dpdf(Result<Box<DpdfEngine>, String>),
}

/// Carries a `Loaded` from the loader thread to the audio thread, once.
struct HandOver(Loaded);

// SAFETY: tract's run states hold `Rc`s, so the engines are not `Send`. Moving one is sound
// because no clone of those `Rc`s stays behind on the loader thread: it builds the whole engine
// there, never runs the model (neither `load` processes a frame), tract's thread-locals hold
// only scratch memory of their own (per-session scratch, created when a session runs), and the
// thread ends right after the send. The channel orders
// the build before any use. Re-check on a tract upgrade: a thread-local caching a model `Rc`
// (or a warm-up run in `load`) would break this.
unsafe impl Send for HandOver {}

struct RnNoiseEngine {
  denoiser: Box<DenoiseState<'static>>,
  input: FrameInput,
  output: FrameOutput,
  /// Previous dry frame: RNNoise output lags its input by one frame, so the dry
  /// signal for the strength blend must be delayed by the same amount (else comb filtering).
  prev_dry: [f32; FRAME_SIZE],
}

struct DeepFilterEngine {
  model: DfTract,
  input: FrameInput,
  output: FrameOutput,
  in_frame: Array2<f32>,
  out_frame: Array2<f32>,
  error_logged: bool,
}

/// RNNoise run only for its voice probability (see `vad`, `in_net`, `out_net`).
struct VoiceNet {
  state: VoiceModel,
  input: FrameInput,
  frame: [f32; FRAME_SIZE],
  scratch: [f32; FRAME_SIZE],
}

/// The 2017 network (`out_net`, `ClickRescue`) or RNNoise 0.2 (`in_net`).
enum VoiceModel {
  Old(Box<DenoiseState<'static>>),
  New(Box<DenoiseState2>),
}

/// Speech decision from RNNoise's per-frame voice probability, with hysteresis. The RNNoise
/// denoiser hears the raw input anyway, so its own probability keys `vad` (a second pass on the
/// same input cost ~0.26 % of a core). Behind a heavy model `vad` needs its own pass: keying it
/// on the heavy model's output instead lost laughter (DFN3 removes part of it; the gate then
/// closed on it, -11 dB on the private scenario take).
struct VoiceHysteresis {
  /// Consecutive speech frames so far.
  run: u32,
  /// Frames left in which a weaker frame (above `keep_prob`) still counts as speech.
  active_left: u32,
  keep_prob: f32,
  active_frames: u32,
  open_prob: f32,
  min_frames: u32,
}

/// Automatic speech gate: open while a detector hears speech, plus a hold; fades instead of
/// chopping, and closed only attenuates (`GATE_FLOOR_DB`). No level threshold, so it works the
/// same for a loud or a quiet voice (day vs. evening). Classic attack/hold/release keyed by a VAD.
/// Tuned to never cut a voice; some loud keys get through (see `OUT_VOICE_PROB`).
/// Fixed delay (ring buffer); empty = no delay. Allocates only when its length changes.
struct DelayLine {
  buf: Vec<f32>,
  pos: usize,
}

struct NoiseGate {
  enabled: bool,
  hold_left: usize,
  gain: f32,
  floor: f32,
}

/// DeepFilterNet 3 rates a frame with a loud click in it (a mouse button next to the mic) as
/// noise and cuts the voice in it with the click: speech at -30 dBFS, clicks hammered at -3 dBFS
/// peak lost > 15 dB in 16 % of the frames with a click (`click_report`; DPDFNet 2 %, RNNoise 0).
/// No DFN3 setting avoids it (its zero-mask threshold is below the model's -15 dB LSNR floor,
/// and a lower attenuation limit only halves it and every pause gets noisier). With the rescue:
/// 5.8 % (gate auto); a raw take of the developer typing while talking (`keyboard_call_report`):
/// speech frames cut 8.0 -> 1.4 % (DPDFNet 3.3 %), pauses unchanged. So RNNoise, aligned
/// to DFN3's latency, runs next to it, and while speech is heard the output crossfades to RNNoise
/// where DFN3 is far under RNNoise. "Speech heard" comes from a voice detector on DFN3's own
/// output: the keys are gone there and the voice stays in most frames, so the hold covers the
/// frames it cut. The raw-input detector would not do: loud keys alone scored two speech frames
/// in a row there and pauses lost 28 dB of key suppression. Cost: two RNNoise passes (~0.5 % of a
/// core); in rescued frames the click comes through as RNNoise leaves it (voice over keys).
struct ClickRescue {
  /// Voice detector on DFN3's output (before the rescue).
  net: VoiceNet,
  hold_left: usize,
  /// Energy envelopes of the DFN3 output and RNNoise's.
  env_dfn: f32,
  env_rnn: f32,
  /// RNNoise's share of the output.
  mix: f32,
}

impl Default for AiVoiceIsolator {
  fn default() -> Self {
    Self::new()
  }
}

impl AiVoiceIsolator {
  pub fn new() -> Self {
    Self {
      rnnoise: RnNoiseEngine::new(),
      deepfilter: None,
      df_error: None,
      dpdfnet: None,
      dpdf_error: None,
      overloaded: false,
      selected: DenoiseModel::RnNoise,
      active: DenoiseModel::RnNoise,
      strength: 0.75,
      df_pre_gain: 1.0,
      gate: NoiseGate::new(true),
      vad: VoiceHysteresis::new(),
      vad_out: VoiceHysteresis::new_output(),
      in_net: VoiceNet::new_input(),
      out_net: VoiceNet::new(),
      soft_net: Vad48k::new().map_err(|e| log::error!("[Denoise] Silero voice detector failed to load: {e}")).ok(),
      soft_voice_prob: SOFT_VOICE_PROB,
      soft_recent: [0.0; 2],
      sleep: Sleep { asleep: false, takeover: None, quiet_samples: 0, delay: DelayLine::new() },
      rescue: ClickRescue::new(),
      in_prob: -1.0,
      out_prob: -1.0,
      rescue_prob: -1.0,
      soft_prob: -1.0,
      floor_segments: [f32::INFINITY; FLOOR_SEGMENTS],
      floor_pos: 0,
      floor_segment_samples: 0,
      level_power: 0.0,
      active_run: 0,
      sleep_after: SLEEP_AFTER_SAMPLES,
      spare: Vec::with_capacity(4096),
      lookahead: false,
      #[cfg(test)]
      lookahead_override: None,
      #[cfg(test)]
      no_rescue: false,
      delay: DelayLine::new(),
      bypassed: false,
      heard_voice: false,
      loading: None,
      mute_left: 0,
    }
  }

  pub fn set_strength(&mut self, strength: f32) {
    self.strength = strength.clamp(0.0, 1.0);
  }

  /// Shared-memory gate setting: at or below `GATE_OFF_DB` = off, at or above
  /// `GATE_LOOKAHEAD_DB` = automatic with lookahead, anything between = automatic.
  pub fn set_gate_threshold_db(&mut self, db: f32) {
    self.gate.enabled = db > GATE_OFF_DB;
    self.lookahead = db >= GATE_LOOKAHEAD_DB;
  }

  /// Closed gate `extra_db` (0..=`GATE_FLOOR_EXTRA_MAX_DB`) under `GATE_FLOOR_DB`: the leveler's
  /// boost after the gate, so pauses stay where they were without it. Takes effect at once (the
  /// gate gain glides to the new floor).
  /// DeepFilterNet cuts a voice that reaches it too quiet: a mic at -54 dBFS voice level lost
  /// 45.6 dB (the 2026-11-09 capture, replayed offline), +4 dB on the input 16.6 dB, +10 dB
  /// 1.4 dB; the same voice at -47.5 dBFS passes untouched, so the cliff is a few dB wide.
  /// RNNoise and DPDFNet pass the quiet voice (-0.7 / -1.8 dB). So the leveler's boost (the
  /// voice is quiet exactly when it is large) is applied to DeepFilterNet's input and removed
  /// from its output: the model sees the voice at a normal level, everything else sees it as
  /// before. The leveler follows it a block late (its gain moves in ~100 ms).
  pub fn set_pre_gain_db(&mut self, db: f32) {
    let db = if db.is_finite() { db.clamp(0.0, DF_PRE_GAIN_MAX_DB) } else { 0.0 };
    self.df_pre_gain = 10f32.powf(db / 20.0);
  }

  pub fn set_gate_floor_extra_db(&mut self, extra_db: f32) {
    let extra = if extra_db.is_finite() { extra_db.clamp(0.0, GATE_FLOOR_EXTRA_MAX_DB) } else { 0.0 };
    self.gate.floor = 10f32.powf((GATE_FLOOR_DB - extra) / 20.0);
  }

  /// Delay the gate lookahead adds on top of `model`, in ms (only while the gate is on).
  pub fn lookahead_ms(model: DenoiseModel) -> f32 {
    lookahead_samples(model) as f32 / 48.0
  }

  /// Choose the model. A heavy model is loaded on first request (blocking, 0.1-0.2 s): only
  /// before audio runs. If it fails, RNNoise keeps running and the error is kept for the UI/log.
  pub fn set_model(&mut self, model: DenoiseModel) {
    self.select(model);
    if self.loading.is_none()
      && let Some((_, load)) = self.missing_load()
    {
      self.store(load());
    }
  }

  /// `set_model` for the real-time thread: never blocks. A heavy model not loaded yet loads on
  /// a short-lived thread while RNNoise keeps running, and takes over in the block after it
  /// arrives (loading in the audio thread stalled the stream, and apps with it, for 0.1-0.2 s).
  pub fn request_model(&mut self, model: DenoiseModel) {
    self.select(model);
    self.start_load();
  }

  pub fn selected_model(&self) -> DenoiseModel {
    self.selected
  }

  /// Give up on the heavy model for now (the caller saw it miss the real-time deadline).
  pub fn fall_back_overloaded(&mut self) {
    if self.active.is_heavy() {
      log::warn!("[Denoise] {:?} too slow for real time on this CPU, switching to RNNoise", self.active);
      self.overloaded = true;
    }
  }

  /// Why the selected model is not running (None if it is, or RNNoise is selected).
  pub fn limit(&self) -> DenoiseLimit {
    let load_error = match self.selected {
      DenoiseModel::DeepFilterNet => self.df_error.is_some(),
      DenoiseModel::DpdfNet => self.dpdf_error.is_some(),
      _ => return DenoiseLimit::None,
    };
    if load_error {
      DenoiseLimit::LoadFailed
    } else if self.overloaded {
      DenoiseLimit::Overloaded
    } else {
      DenoiseLimit::None
    }
  }

  /// Delay the model adds to the microphone, in ms (0 for Unknown).
  pub fn latency_ms(model: DenoiseModel) -> f32 {
    latency_samples(model) as f32 / 48.0
  }

  /// Model that is actually processing audio right now.
  pub fn active_model(&self) -> DenoiseModel {
    self.active
  }

  /// The strict detector heard speech in the block just processed. That audio leaves this stage
  /// `output_lead_samples` later.
  pub fn heard_voice(&self) -> bool {
    self.heard_voice
  }

  /// Model latency + gate lookahead (0 while bypassed). The same while a heavy model sleeps.
  pub fn output_lead_samples(&self) -> usize {
    if self.bypassed { 0 } else { latency_samples(self.active) + self.delay.len() }
  }

  /// Gate lookahead in the output right now (0 while the gate or the lookahead is off).
  pub fn lookahead_now_samples(&self) -> usize {
    if self.bypassed { 0 } else { self.delay.len() }
  }

  /// Lookahead for the model producing the output (keyed like its latency).
  fn lookahead_len(&self) -> usize {
    #[cfg(test)]
    if let Some(n) = self.lookahead_override {
      return n;
    }
    lookahead_samples(self.active)
  }

  /// The heavy model is stopped for a pause (RNNoise stands in).
  pub fn asleep(&self) -> bool {
    self.sleep.asleep && self.sleep.takeover.is_none()
  }

  /// Detector, gate and rescue state after the last block, for the debug capture.
  pub fn debug_state(&self) -> DenoiseDebug {
    DenoiseDebug {
      in_prob: self.in_prob,
      out_prob: self.out_prob,
      rescue_prob: self.rescue_prob,
      soft_prob: self.soft_prob,
      vad_run: self.vad.run,
      vad_active_left: self.vad.active_left,
      vad_out_active_left: self.vad_out.active_left,
      gate_enabled: self.gate.enabled,
      gate_gain: self.gate.gain,
      gate_hold_left: self.gate.hold_left,
      gate_floor: self.gate.floor,
      df_pre_gain: self.df_pre_gain,
      rescue_mix: self.rescue.mix,
      rescue_hold_left: self.rescue.hold_left,
      bypassed: self.bypassed,
      quiet_samples: self.sleep.quiet_samples,
      takeover: self.sleep.takeover.is_some(),
    }
  }

  /// Process microphone samples in place.
  /// RNNoise adds a fixed latency of one frame (10 ms) plus the model's own frame;
  /// every output sample is processed, whatever the block size.
  pub fn process_samples(&mut self, samples: &mut [f32]) {
    (self.in_prob, self.out_prob, self.rescue_prob, self.soft_prob) = (-1.0, -1.0, -1.0, -1.0);
    self.poll_load();
    let loaded = match self.selected {
      DenoiseModel::DeepFilterNet => self.deepfilter.is_some(),
      DenoiseModel::DpdfNet => self.dpdfnet.is_some(),
      _ => false,
    };
    let wanted = if loaded && !self.overloaded { self.selected } else { DenoiseModel::RnNoise };
    if wanted != self.active {
      // Fresh state for the engine taking over; its FIFOs may hold stale audio.
      self.reset_engines();
      self.active = wanted;
    }

    if self.strength <= 0.001 {
      // Audio passes untouched, but a detector (`in_net`, ~2.8 % of a core) keeps running:
      // the leveler and the automatic input gain learn only from speech it hears.
      self.bypassed = true;
      let (vad, in_prob, mut voice) = (&mut self.vad, &mut self.in_prob, false);
      let (open, keep) = self.in_net.thresholds();
      self.in_net.process(samples, |p| {
        *in_prob = in_prob.max(p);
        voice |= vad.frame_at(p, open, keep);
      });
      self.heard_voice = voice;
      return;
    }
    if self.bypassed {
      // Stale FIFO content from before the bypass would replay old audio.
      self.reset();
    }

    let (active, strength, gate_on) = (self.active, self.strength, self.gate.enabled);
    let heavy = active.is_heavy();
    let latency = latency_samples(active);
    let activity = heavy && self.note_activity(samples);
    // Speech again while handing a pause to RNNoise, before RNNoise is heard at all: stay with
    // the heavy model (finishing the handover put the word's onset through RNNoise, and the
    // wake-up takeover after it cost another model latency).
    if self.sleep.takeover.is_some_and(|t| t.to_sleep && t.done < t.fade_at) && (self.heard_voice || activity) {
      self.sleep.takeover = None;
    }
    if heavy && self.sleep.takeover.is_none() {
      if !self.sleep.asleep && self.sleep.quiet_samples >= self.sleep_after {
        self.start_takeover(true, latency);
      } else if self.sleep.asleep && (self.heard_voice || activity) {
        self.start_takeover(false, latency);
      }
    }
    let takeover = self.sleep.takeover;
    let heavy_on = heavy && (!self.sleep.asleep || takeover.is_some());
    #[cfg(test)]
    let rescue_off = self.no_rescue;
    #[cfg(not(test))]
    let rescue_off = false;
    let rescue_on = heavy_on && takeover.is_none() && active == DenoiseModel::DeepFilterNet && !rescue_off;
    let rnnoise_on = !heavy_on || takeover.is_some() || rescue_on;
    if rescue_on {
      // No-op unless the length changed (model switch): aligns RNNoise with DFN3's output.
      self.sleep.delay.set_len(latency.saturating_sub(RNNOISE_LATENCY_SAMPLES));
    }
    let n = samples.len();
    if heavy_on && rnnoise_on && self.spare.len() < n {
      self.spare.resize(n, 0.0); // only for a block over the preallocated 85 ms
    }

    let Self {
      vad,
      vad_out,
      in_net,
      out_net,
      soft_net,
      soft_voice_prob,
      soft_recent,
      rnnoise,
      deepfilter,
      dpdfnet,
      sleep,
      spare,
      mute_left,
      rescue,
      df_pre_gain,
      in_prob,
      out_prob,
      rescue_prob,
      soft_prob,
      ..
    } = self;
    let (mut voice, mut voice_out, mut voice_soft) = (false, false, false);
    if let (true, Some(net)) = (gate_on || rescue_on, soft_net.as_mut()) {
      net.process(samples, |p| {
        *soft_prob = soft_prob.max(p);
        voice_soft |= p > *soft_voice_prob;
        *soft_recent = [soft_recent[1], p];
      });
    }
    // The chunk just reported and the one before cover what DFN3 puts out now (its latency is
    // 40 ms, a chunk 32 ms).
    let soft_voice_now = soft_net.is_none() || soft_recent[0].max(soft_recent[1]) > RESCUE_SOFT_PROB;
    // Raw-input detector, a model latency ahead of the output (opens on word onsets in time):
    // RNNoise's own voice probability while it denoises alone, else `in_net` (RNNoise 0.2).
    if heavy_on {
      let (open, keep) = in_net.thresholds();
      in_net.process(samples, |p| {
        *in_prob = in_prob.max(p);
        voice |= vad.frame_at(p, open, keep);
      });
    }
    // During a takeover RNNoise works on a copy, the heavy model in place.
    let stand_in = match spare.get_mut(..n) {
      Some(copy) if heavy_on && rnnoise_on => {
        copy.copy_from_slice(samples);
        Some(copy)
      }
      _ => None,
    };
    if heavy_on {
      match (active, deepfilter.as_mut(), dpdfnet.as_mut()) {
        (DenoiseModel::DeepFilterNet, Some(df), _) => df.process(samples, strength, *df_pre_gain),
        (DenoiseModel::DpdfNet, _, Some(dp)) => dp.process(samples, atten_lim(strength)),
        _ => {}
      }
      let muted = (*mute_left).min(n);
      samples.iter_mut().take(muted).for_each(|s| *s = 0.0);
      *mute_left -= muted;
    }
    if rnnoise_on {
      let buf: &mut [f32] = match stand_in {
        Some(copy) => copy,
        None => &mut *samples,
      };
      rnnoise.process(buf, strength, |p| {
        if !heavy_on {
          *in_prob = in_prob.max(p);
          voice |= vad.frame(p);
        }
      });
      if heavy {
        sleep.delay.process(buf); // standing in: delayed to the heavy model's latency
      }
    }
    if let (Some(t), true, Some(copy)) = (takeover, heavy_on && rnnoise_on, spare.get(..n)) {
      for (i, (out, &stand)) in samples.iter_mut().zip(copy).enumerate() {
        // Newcomer's share: 0 until its first real sample is out, then a short crossfade.
        let w = ((t.done + i).saturating_sub(t.fade_at) as f32 / TAKEOVER_FADE_SAMPLES as f32).min(1.0);
        let (old, new) = if t.to_sleep { (*out, stand) } else { (stand, *out) };
        *out = old + (new - old) * w;
      }
    }
    if let (true, Some(stand)) = (rescue_on, spare.get(..n)) {
      *rescue_prob = rescue.process(samples, stand, soft_voice_now);
    } else {
      rescue.reset();
    }
    // The output detector, in step with the output (its hold needs no latency on top). Not while
    // a heavy model sleeps: it would hear the keys RNNoise leaves in (DFN3, loud typing: gate
    // open 25 % instead of 1 %); the input detector alone wakes the model.
    if gate_on && (heavy_on || !heavy) {
      out_net.process(samples, |p| {
        *out_prob = out_prob.max(p);
        voice_out |= vad_out.frame(p);
      });
    }
    if let Some(mut t) = takeover {
      t.done += n;
      self.sleep.takeover = if t.done >= t.fade_at + TAKEOVER_FADE_SAMPLES {
        self.sleep.asleep = t.to_sleep;
        None
      } else {
        Some(t)
      };
    }
    self.sleep.quiet_samples = if voice || activity { 0 } else { self.sleep.quiet_samples.saturating_add(n) };
    self.heard_voice = voice;
    // Lookahead: the gated audio is older than what the detectors just heard, so the gate opens
    // that much before an onset; the hold covers the delay too, so word tails stay as long.
    self.delay.set_len(if gate_on && self.lookahead { self.lookahead_len() } else { 0 });
    self.delay.process(samples);
    let hold = match (voice || voice_soft, voice_out) {
      (true, _) => GATE_HOLD_SAMPLES + latency + self.delay.len(),
      (false, true) => GATE_HOLD_SAMPLES + self.delay.len(),
      (false, false) => 0,
    };
    self.gate.process(samples, hold);
  }

  /// Cold start: `reset_engines`, and fresh detector states. Allocates: only on user actions
  /// (denoiser switched back on, model change), never on a pause (`flush`).
  pub fn reset(&mut self) {
    self.reset_engines();
    self.in_net.reset();
    self.out_net = VoiceNet::new();
    if let Some(net) = self.soft_net.as_mut() {
      net.reset();
    }
    self.reset_timing();
  }

  /// `reset` for the real-time thread (the host paused the chain): drops all audio buffered from
  /// before, keeps the loaded models and their network states, allocates nothing.
  pub fn flush(&mut self) {
    self.sleep.clear();
    self.rescue.reset();
    self.rnnoise.flush();
    if let Some(df) = self.deepfilter.as_mut() {
      df.reset();
    }
    if let Some(dp) = self.dpdfnet.as_mut() {
      dp.reset(); // cold: its state holds spectra from before the pause, beyond its latency
    }
    // DeepFilterNet's state lives in tract tensors inside the fork (clearing them allocates):
    // its first latency of output, all from before the pause, is silenced instead.
    self.mute_left = if self.active.is_heavy() { latency_samples(self.active) } else { 0 };
    self.in_net.input.clear();
    self.out_net.input.clear();
    if let Some(net) = self.soft_net.as_mut() {
      net.flush();
    }
    self.reset_timing();
  }

  /// What follows the output timing: gate, detector hysteresis, lookahead, bypass.
  fn reset_timing(&mut self) {
    self.gate = NoiseGate::new(self.gate.enabled);
    self.vad = VoiceHysteresis::new();
    self.vad_out = VoiceHysteresis::new_output();
    self.delay.clear();
    self.bypassed = false;
  }

  /// Tracks the raw input's noise floor; true while the input has been over it by
  /// `ACTIVITY_OVER_FLOOR_DB` for `ACTIVITY_MIN_SAMPLES` (anything but a quiet room).
  fn note_activity(&mut self, samples: &[f32]) -> bool {
    if samples.is_empty() {
      return false;
    }
    let power = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    if !power.is_finite() {
      return false; // a NaN block must not stick in the level or the floor
    }
    let k = 1.0 - (-(samples.len() as f32) / ACTIVITY_SMOOTH_SAMPLES).exp();
    self.level_power += (power - self.level_power) * k;
    let level_db = 10.0 * (self.level_power + 1e-12).log10();
    let db = (10.0 * (power + 1e-12).log10()).max(FLOOR_MIN_DB);
    if self.floor_segment_samples >= FLOOR_SEGMENT_SAMPLES {
      self.floor_segment_samples = 0;
      self.floor_pos = (self.floor_pos + 1) % FLOOR_SEGMENTS;
      if let Some(seg) = self.floor_segments.get_mut(self.floor_pos) {
        *seg = f32::INFINITY;
      }
    }
    if let Some(seg) = self.floor_segments.get_mut(self.floor_pos) {
      *seg = seg.min(db);
    }
    self.floor_segment_samples += samples.len();
    let floor = self.floor_segments.iter().copied().fold(f32::INFINITY, f32::min);
    self.active_run = if level_db > floor + ACTIVITY_OVER_FLOOR_DB { self.active_run + samples.len() } else { 0 };
    self.active_run >= ACTIVITY_MIN_SAMPLES
  }

  /// Start handing a pause to RNNoise (`to_sleep`, its buffers emptied, the sleep delay set to the
  /// heavy model's latency) or speech back to the heavy model (its buffers emptied).
  fn start_takeover(&mut self, to_sleep: bool, latency: usize) {
    if to_sleep {
      self.rnnoise.restart();
      self.sleep.delay.set_len(latency.saturating_sub(RNNOISE_LATENCY_SAMPLES));
      self.sleep.delay.clear();
    } else {
      // Its FIFOs hold audio from before the pause; the model's own state warms up again.
      if let Some(df) = self.deepfilter.as_mut() {
        df.reset();
      }
      if let Some(dp) = self.dpdfnet.as_mut() {
        dp.restart();
      }
      self.in_net.input.clear();
    }
    let fade_at = if to_sleep { latency.max(STAND_IN_WARMUP_SAMPLES) } else { latency };
    self.sleep.takeover = Some(Takeover { to_sleep, done: 0, fade_at });
  }

  fn select(&mut self, model: DenoiseModel) {
    if model != self.selected {
      // Picking a model again = try it again, a load that failed included.
      self.selected = model;
      self.forget_failure();
    }
  }

  /// The user picked the selected model again (`denoise_retry_seq`): one that failed to load or
  /// was too slow gets another try. Never blocks, like `request_model`.
  pub fn retry(&mut self) {
    self.forget_failure();
    self.start_load();
  }

  fn forget_failure(&mut self) {
    self.overloaded = false;
    match self.selected {
      DenoiseModel::DeepFilterNet => self.df_error = None,
      DenoiseModel::DpdfNet => self.dpdf_error = None,
      _ => {}
    }
  }

  /// Loader for the selected heavy model, if it is neither loaded nor known to fail.
  fn missing_load(&self) -> Option<(DenoiseModel, fn() -> Loaded)> {
    match self.selected {
      DenoiseModel::DeepFilterNet if self.deepfilter.is_none() && self.df_error.is_none() => {
        Some((DenoiseModel::DeepFilterNet, load_deepfilter))
      }
      DenoiseModel::DpdfNet if self.dpdfnet.is_none() && self.dpdf_error.is_none() => {
        Some((DenoiseModel::DpdfNet, load_dpdfnet))
      }
      _ => None,
    }
  }

  fn start_load(&mut self) {
    if self.loading.is_some() {
      return; // one at a time; `poll_load` starts the next when this one lands
    }
    let Some((model, load)) = self.missing_load() else {
      return;
    };
    let (tx, rx) = mpsc::channel();
    // Receiver gone (stream ended) = the send fails and the engine is dropped on this thread.
    let spawned =
      std::thread::Builder::new().name("denoise-model-load".into()).spawn(move || tx.send(HandOver(load())));
    match spawned {
      Ok(_) => self.loading = Some((model, rx)),
      Err(e) => self.store(load_failed(model, format!("could not start the loader thread: {e}"))),
    }
  }

  /// Non-blocking: takes over a model the loader thread finished.
  fn poll_load(&mut self) {
    let Some((model, rx)) = self.loading.as_ref() else {
      return;
    };
    let loaded = match rx.try_recv() {
      Ok(HandOver(loaded)) => loaded,
      Err(TryRecvError::Empty) => return,
      Err(TryRecvError::Disconnected) => load_failed(*model, "the loader thread stopped".into()),
    };
    self.loading = None;
    self.store(loaded);
    self.start_load(); // another heavy model may have been selected meanwhile
  }

  fn store(&mut self, loaded: Loaded) {
    match loaded {
      Loaded::DeepFilter(r) => put(&mut self.deepfilter, &mut self.df_error, r),
      Loaded::Dpdf(r) => put(&mut self.dpdfnet, &mut self.dpdf_error, r),
    }
  }

  /// Fresh engines for a model switch: sleep cleared, nothing left to silence, RNNoise's network
  /// state new, the heavy models' buffers emptied.
  fn reset_engines(&mut self) {
    self.sleep.clear();
    self.mute_left = 0;
    self.rnnoise.reset();
    if let Some(df) = self.deepfilter.as_mut() {
      df.reset();
    }
    if let Some(dp) = self.dpdfnet.as_mut() {
      dp.reset();
    }
  }
}

/// Loads a heavy engine, logging the outcome (also on the loader thread; audiodg has no logger).
fn load_logged<E>(name: &str, load: fn() -> Result<E, String>) -> Result<Box<E>, String> {
  let t0 = Instant::now();
  match load() {
    Ok(engine) => {
      log::info!("[Denoise] {name} model loaded in {:?}", t0.elapsed());
      Ok(Box::new(engine))
    }
    Err(e) => {
      log::error!("[Denoise] {name} failed to load, staying on RNNoise: {e}");
      Err(e)
    }
  }
}

fn load_deepfilter() -> Loaded {
  Loaded::DeepFilter(load_logged("DeepFilterNet 3", DeepFilterEngine::load))
}

fn load_dpdfnet() -> Loaded {
  Loaded::Dpdf(load_logged("DPDFNet", DpdfEngine::load))
}

/// A load of `model` that could not run (no thread, or the loader died).
fn load_failed(model: DenoiseModel, why: String) -> Loaded {
  log::error!("[Denoise] {model:?} failed to load, staying on RNNoise: {why}");
  match model {
    DenoiseModel::DpdfNet => Loaded::Dpdf(Err(why)),
    _ => Loaded::DeepFilter(Err(why)),
  }
}

fn put<E>(slot: &mut Option<Box<E>>, error: &mut Option<String>, loaded: Result<Box<E>, String>) {
  match loaded {
    Ok(engine) => *slot = Some(engine),
    Err(e) => *error = Some(e),
  }
}

/// Strength -> share of the dry signal the heavy models mix back (None = unlimited).
fn atten_lim(strength: f32) -> Option<f32> {
  if strength >= 0.999 {
    None
  } else {
    let db = DF_MIN_ATTEN_DB + strength * (DF_MAX_ATTEN_DB - DF_MIN_ATTEN_DB);
    Some(10f32.powf(-db / 20.0))
  }
}

impl Sleep {
  fn clear(&mut self) {
    (self.asleep, self.takeover, self.quiet_samples) = (false, None, 0);
    self.delay.clear();
  }
}

impl RnNoiseEngine {
  fn new() -> Self {
    Self {
      denoiser: DenoiseState::new(),
      input: FrameInput::new(FRAME_SIZE, FRAME_SIZE),
      output: FrameOutput::new(FRAME_SIZE),
      prev_dry: [0.0; FRAME_SIZE],
    }
  }

  /// `on_prob`: RNNoise's voice probability of each input frame it completes.
  fn process(&mut self, samples: &mut [f32], strength: f32, mut on_prob: impl FnMut(f32)) {
    self.input.push(samples);

    let mut temp_out = [0.0f32; FRAME_SIZE];
    let mut temp_in = [0.0f32; FRAME_SIZE];
    // Linear mix: the default 0.75 keeps 25 % dry, so RNNoise takes at most ~12 dB off any
    // noise. Kept on purpose. A dB mix (dry 6 dB down to 20 / 30 dB, `private_report`,
    // auto + lookahead, 2026-11-08) made pauses 19 / 25 dB quieter instead of 12-14, but the
    // soft syllables the gate cuts rose 1.6-2.4 -> 2.6-4.3 / 3.8-5.9 %. A few keys let through
    // beat one cut letter. A stricter output detector for RNNoise (0.3 -> 0.5: keys open the
    // gate 18 % instead of 31 %) cost +0.3-0.4 points too and was dropped as well.
    let (wet_gain, dry_gain) = (strength, 1.0 - strength);

    while let Some(frame) = self.input.window() {
      for (dst, src) in temp_in.iter_mut().zip(frame) {
        *dst = src * I16_SCALE;
      }
      self.input.advance();

      on_prob(self.denoiser.process_frame(&mut temp_out, &temp_in));

      for (wet, dry) in temp_out.iter().zip(self.prev_dry.iter()) {
        self.output.push((wet_gain * wet + dry_gain * dry) / I16_SCALE);
      }
      self.prev_dry = temp_in;
    }

    self.output.drain_into(samples);
  }

  fn reset(&mut self) {
    self.denoiser = DenoiseState::new();
    self.restart();
  }

  /// `restart`, and the audio the network still holds (input filter, window overlap, pitch
  /// history) pushed out by silence, without a new state (`DenoiseState::new` and even a clone
  /// allocate). The recurrent state stays about as warm as it was.
  fn flush(&mut self) {
    let (silence, mut out) = ([0.0f32; FRAME_SIZE], [0.0f32; FRAME_SIZE]);
    for _ in 0..RNNOISE_FLUSH_FRAMES {
      self.denoiser.process_frame(&mut out, &silence);
    }
    self.restart();
  }

  /// Fresh buffers, the network state kept: a cold RNNoise rates the first frames of typing as
  /// speech, so a fresh one at every pause woke the heavy model again (gate open 6-7 % on keys).
  fn restart(&mut self) {
    self.input.clear();
    self.output.clear();
    self.prev_dry = [0.0; FRAME_SIZE];
  }
}

impl DeepFilterEngine {
  fn load() -> Result<Self, String> {
    let params =
      RuntimeParams::default_with_ch(1).with_thresholds(DF_MIN_DB_THRESH, DF_MAX_DB_ERB_THRESH, DF_MAX_DB_DF_THRESH);
    let model = DfTract::new(DfParams::default(), &params).map_err(|e| format!("{e:#}"))?;
    let hop = model.hop_size.max(1);
    if model.sr != 48_000 {
      return Err(format!("unexpected model sample rate {}", model.sr));
    }
    Ok(Self {
      model,
      input: FrameInput::new(hop, hop),
      output: FrameOutput::new(hop),
      in_frame: Array2::zeros((1, hop)),
      out_frame: Array2::zeros((1, hop)),
      error_logged: false,
    })
  }

  fn process(&mut self, samples: &mut [f32], strength: f32, pre_gain: f32) {
    // Strength maps to the model's own attenuation limit (no dry/wet blend: DF's output
    // is delayed by its look-ahead, a blend would comb-filter).
    self.model.atten_lim = atten_lim(strength);

    self.input.push_scaled(samples, pre_gain);
    while let Some(frame) = self.input.window() {
      for (dst, src) in self.in_frame.iter_mut().zip(frame) {
        *dst = *src;
      }
      self.input.advance();
      match self.model.process(self.in_frame.view(), self.out_frame.view_mut()) {
        Ok(_) => self.output.extend(self.out_frame.iter().copied()),
        Err(e) => {
          if !self.error_logged {
            log::error!("[Denoise] DeepFilterNet frame failed, passing audio through: {e:#}");
            self.error_logged = true;
          }
          self.output.extend(self.in_frame.iter().copied());
        }
      }
    }
    self.output.drain_into(samples);
    samples.iter_mut().for_each(|s| *s /= pre_gain);
  }

  fn reset(&mut self) {
    self.input.clear();
    self.output.clear();
  }
}

impl VoiceNet {
  /// The 2017 network (`out_net`, `ClickRescue`).
  fn new() -> Self {
    Self::with_model(VoiceModel::Old(DenoiseState::new()))
  }

  /// `in_net`: RNNoise 0.2 (Full). If its embedded weights fail to load (never seen; a corrupt
  /// build), the 2017 network instead, so the chain always has a detector.
  fn new_input() -> Self {
    let model = match DenoiseState2::with_model(rnnoise2::Model::Full) {
      Ok(state) => VoiceModel::New(state),
      Err(e) => {
        log::error!("[Denoise] RNNoise 0.2 voice detector failed to load, using the 2017 one: {e}");
        VoiceModel::Old(DenoiseState::new())
      }
    };
    Self::with_model(model)
  }

  fn with_model(state: VoiceModel) -> Self {
    Self { state, input: FrameInput::new(FRAME_SIZE, FRAME_SIZE), frame: [0.0; FRAME_SIZE], scratch: [0.0; FRAME_SIZE] }
  }

  /// Open / keep thresholds of the raw-input `vad` for this network's probability.
  fn thresholds(&self) -> (f32, f32) {
    match self.state {
      VoiceModel::Old(_) => (VOICE_PROB, VOICE_KEEP_PROB),
      VoiceModel::New(_) => (IN_NET_VOICE_PROB, IN_NET_KEEP_PROB),
    }
  }

  /// Calls `on_prob` with the voice probability of each frame completed in this block
  /// (RNNoise 0.2: of the frame before it, see `in_net`).
  fn process(&mut self, samples: &[f32], mut on_prob: impl FnMut(f32)) {
    self.input.push(samples);
    while let Some(frame) = self.input.window() {
      for (dst, src) in self.frame.iter_mut().zip(frame) {
        *dst = src * I16_SCALE;
      }
      self.input.advance();
      let prob = match &mut self.state {
        VoiceModel::Old(state) => state.process_frame(&mut self.scratch, &self.frame),
        VoiceModel::New(state) => state.voice_probability(&self.frame),
      };
      on_prob(prob);
    }
  }

  /// Fresh network state and no buffered audio. The 2017 network allocates a new state (user
  /// actions only); RNNoise 0.2 clears its own without allocating or re-parsing its weights.
  fn reset(&mut self) {
    match &mut self.state {
      VoiceModel::Old(state) => *state = DenoiseState::new(),
      VoiceModel::New(state) => state.reset(),
    }
    self.input.clear();
  }
}

impl VoiceHysteresis {
  fn new() -> Self {
    Self {
      run: 0,
      active_left: 0,
      keep_prob: VOICE_KEEP_PROB,
      active_frames: VOICE_ACTIVE_FRAMES,
      open_prob: VOICE_PROB,
      min_frames: VOICE_MIN_FRAMES,
    }
  }

  /// Detector for the denoised output, see `OUT_VOICE_PROB`.
  fn new_output() -> Self {
    Self {
      open_prob: OUT_VOICE_PROB,
      min_frames: OUT_VOICE_MIN_FRAMES,
      keep_prob: OUT_VOICE_KEEP_PROB,
      active_frames: OUT_VOICE_ACTIVE_FRAMES,
      ..Self::new()
    }
  }

  /// One frame's voice probability -> is it speech.
  fn frame(&mut self, prob: f32) -> bool {
    self.frame_at(prob, self.open_prob, self.keep_prob)
  }

  /// `frame` with the thresholds of the network that scored `prob` (`VoiceNet::thresholds`).
  fn frame_at(&mut self, prob: f32, open_prob: f32, keep_prob: f32) -> bool {
    self.run = if prob > open_prob { self.run.saturating_add(1) } else { 0 };
    // Hysteresis: opening needs a clear syllable; once talking, a soft one keeps it open.
    let voice = self.run >= self.min_frames || (self.active_left > 0 && prob > keep_prob);
    self.active_left = if voice { self.active_frames } else { self.active_left.saturating_sub(1) };
    voice
  }
}

impl DelayLine {
  fn new() -> Self {
    Self { buf: Vec::new(), pos: 0 }
  }

  fn len(&self) -> usize {
    self.buf.len()
  }

  /// Changing the length restarts from silence: a gap once, when the setting changes.
  fn set_len(&mut self, len: usize) {
    if len != self.buf.len() {
      self.buf = vec![0.0; len];
      self.pos = 0;
    }
  }

  fn clear(&mut self) {
    self.buf.fill(0.0);
    self.pos = 0;
  }

  fn process(&mut self, samples: &mut [f32]) {
    let len = self.buf.len();
    if len == 0 {
      return;
    }
    for s in samples.iter_mut() {
      if let Some(slot) = self.buf.get_mut(self.pos) {
        std::mem::swap(slot, s);
      }
      self.pos = (self.pos + 1) % len;
    }
  }
}

impl NoiseGate {
  fn new(enabled: bool) -> Self {
    Self { enabled, hold_left: 0, gain: 0.0, floor: 10f32.powf(GATE_FLOOR_DB / 20.0) }
  }

  /// `hold`: samples to stay open from now (speech heard in this block), 0 = none heard.
  /// Never shortens a running hold.
  fn process(&mut self, samples: &mut [f32], hold: usize) {
    if !self.enabled {
      return;
    }
    self.hold_left = self.hold_left.max(hold);
    for s in samples.iter_mut() {
      self.hold_left = self.hold_left.saturating_sub(1);
      let (target, coef) = if self.hold_left > 0 { (1.0, GATE_OPEN_COEF) } else { (self.floor, GATE_CLOSE_COEF) };
      self.gain += (target - self.gain) * coef;
      *s *= self.gain;
    }
  }
}

impl ClickRescue {
  fn new() -> Self {
    Self { net: VoiceNet::new(), hold_left: 0, env_dfn: 0.0, env_rnn: 0.0, mix: 0.0 }
  }

  /// `out`: DFN3 output, in place; `rnn`: RNNoise's output of the same samples; `voice_now`:
  /// Silero hears a voice in the raw input around these samples (see `RESCUE_SOFT_PROB`). Returns the
  /// detector's highest voice probability in this block (-1: no frame completed).
  fn process(&mut self, out: &mut [f32], rnn: &[f32], voice_now: bool) -> f32 {
    let ratio = 10f32.powf(RESCUE_RATIO_DB / 10.0);
    let (mut heard, mut prob) = (false, -1.0f32);
    self.net.process(out, |p| {
      prob = prob.max(p);
      heard |= p > RESCUE_VOICE_PROB;
    });
    if heard {
      self.hold_left = out.len() + RESCUE_HOLD_SAMPLES;
    }
    for (d, &r) in out.iter_mut().zip(rnn) {
      self.hold_left = self.hold_left.saturating_sub(1);
      self.env_dfn += (*d * *d - self.env_dfn) * RESCUE_ENV_COEF;
      self.env_rnn += (r * r - self.env_rnn) * RESCUE_ENV_COEF;
      let cut = voice_now && self.hold_left > 0 && self.env_rnn > self.env_dfn * ratio;
      let (target, coef) = if cut { (1.0, RESCUE_IN_COEF) } else { (0.0, RESCUE_OUT_COEF) };
      self.mix += (target - self.mix) * coef;
      if !(self.mix.is_finite() && self.env_dfn.is_finite() && self.env_rnn.is_finite()) {
        self.reset(); // a NaN must not stick
        continue;
      }
      *d += (r - *d) * self.mix;
    }
    prob
  }

  /// Idle again (rescue off: RNNoise standing in, another model). No allocation; the detector
  /// keeps its network state (like `flush`), only its buffered audio goes.
  fn reset(&mut self) {
    self.net.input.clear();
    (self.hold_left, self.env_dfn, self.env_rnn, self.mix) = (0, 0.0, 0.0, 0.0);
  }
}

fn lookahead_samples(model: DenoiseModel) -> usize {
  match model {
    DenoiseModel::RnNoise | DenoiseModel::Unknown(_) => RNNOISE_LOOKAHEAD_SAMPLES,
    DenoiseModel::DeepFilterNet => DF_LOOKAHEAD_SAMPLES,
    DenoiseModel::DpdfNet => DPDF_LOOKAHEAD_SAMPLES,
  }
}

fn latency_samples(model: DenoiseModel) -> usize {
  match model {
    DenoiseModel::RnNoise => RNNOISE_LATENCY_SAMPLES,
    DenoiseModel::DeepFilterNet => DF_LATENCY_SAMPLES,
    DenoiseModel::DpdfNet => DPDF_LATENCY_SAMPLES,
    DenoiseModel::Unknown(_) => 0,
  }
}

#[cfg(test)]
pub(crate) mod tests;
