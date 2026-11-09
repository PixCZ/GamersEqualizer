//! Neural noise suppression: RNNoise (nnnoiseless, light), DeepFilterNet 3 (higher quality,
//! ~2 % of a core) or DPDFNet-2 (best on soft speech, ~11 %, see `dpdfnet`). The heavy models
//! load on first selection (DFN3 ~80 ms, DPDFNet ~120 ms) and then stay in memory. The stream
//! loads its selected model before audio starts (`set_model`); a live switch (`request_model`)
//! loads on a short-lived thread while RNNoise keeps running, so the audio thread never stalls.
//! The raw-input voice detector (`in_net`) runs RNNoise 0.2 (`rnnoise2`), everything else the
//! 2017 RNNoise network.

use crate::dpdfnet::{self, DpdfEngine};
use df::tract::{DfParams, DfTract, RuntimeParams};
use gamersequalizer_common::shm::{DenoiseLimit, DenoiseModel};
use ndarray::Array2;
use nnnoiseless::{DenoiseState, FRAME_SIZE};
use rnnoise2::DenoiseState as DenoiseState2;
use silero_vad::Vad48k;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

/// nnnoiseless (RNNoise) is trained on i16-range samples, not [-1.0, 1.0].
const I16_SCALE: f32 = 32768.0;
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
  /// Heavy model asleep: no speech for `sleep_after` samples, so RNNoise (delayed to the heavy
  /// model's latency) covers the pause and the heavy model stops (DFN3 ~3 %, DPDFNet ~10 % of a
  /// core). Speech on RNNoise's own detector wakes it.
  asleep: bool,
  /// Both run while one takes over from the other (see `Takeover`).
  takeover: Option<Takeover>,
  /// DeepFilterNet only: RNNoise runs next to it and fills in where DFN3 cut a voice (clicks).
  rescue: ClickRescue,
  /// Input samples since the input detector last heard speech or saw activity.
  quiet_samples: usize,
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
  /// RNNoise output delayed to the heavy model's latency while it stands in.
  sleep_delay: DelayLine,
  /// The second path's copy of the block during a handover.
  spare: Vec<f32>,
  /// Heavy model being loaded on the loader thread (one at a time).
  loading: Option<(DenoiseModel, Receiver<HandOver>)>,
  /// Heavy-model output samples still to silence after a `flush`: its recurrent state and
  /// lookahead frames hold the audio from before (one model latency of it).
  mute_left: usize,
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
// only scratch memory of their own (0.21: element-wise temp buffer; 0.23: per-session scratch,
// created when a session runs), and the thread ends right after the send. The channel orders
// the build before any use. Re-check on a tract upgrade: a thread-local caching a model `Rc`
// (or a warm-up run in `load`) would break this.
#[allow(unsafe_code)]
unsafe impl Send for HandOver {}

struct RnNoiseEngine {
  denoiser: Box<DenoiseState<'static>>,
  input_fifo: Vec<f32>,
  output_fifo: Vec<f32>,
  /// Previous dry frame: RNNoise output lags its input by one frame, so the dry
  /// signal for the strength blend must be delayed by the same amount (else comb filtering).
  prev_dry: [f32; FRAME_SIZE],
}

struct DeepFilterEngine {
  model: DfTract,
  hop: usize,
  input_fifo: Vec<f32>,
  output_fifo: Vec<f32>,
  in_frame: Array2<f32>,
  out_frame: Array2<f32>,
  error_logged: bool,
}

/// RNNoise run only for its voice probability (see `vad`, `in_net`, `out_net`).
struct VoiceNet {
  state: VoiceModel,
  fifo: Vec<f32>,
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
      asleep: false,
      takeover: None,
      rescue: ClickRescue::new(),
      quiet_samples: 0,
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
      sleep_delay: DelayLine::new(),
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
    if self.loading.is_none() {
      if let Some((_, load)) = self.missing_load() {
        self.store(load());
      }
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
    if self.bypassed {
      0
    } else {
      latency_samples(self.active) + self.delay.len()
    }
  }

  /// Gate lookahead in the output right now (0 while the gate or the lookahead is off).
  pub fn lookahead_now_samples(&self) -> usize {
    if self.bypassed {
      0
    } else {
      self.delay.len()
    }
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
    self.asleep && self.takeover.is_none()
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
      quiet_samples: self.quiet_samples,
      takeover: self.takeover.is_some(),
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
    if self.takeover.is_some_and(|t| t.to_sleep && t.done < t.fade_at) && (self.heard_voice || activity) {
      self.takeover = None;
    }
    if heavy && self.takeover.is_none() {
      if !self.asleep && self.quiet_samples >= self.sleep_after {
        self.start_takeover(true, latency);
      } else if self.asleep && (self.heard_voice || activity) {
        self.start_takeover(false, latency);
      }
    }
    let takeover = self.takeover;
    let heavy_on = heavy && (!self.asleep || takeover.is_some());
    #[cfg(test)]
    let rescue_off = self.no_rescue;
    #[cfg(not(test))]
    let rescue_off = false;
    let rescue_on = heavy_on && takeover.is_none() && active == DenoiseModel::DeepFilterNet && !rescue_off;
    let rnnoise_on = !heavy_on || takeover.is_some() || rescue_on;
    if rescue_on {
      // No-op unless the length changed (model switch): aligns RNNoise with DFN3's output.
      self.sleep_delay.set_len(latency.saturating_sub(RNNOISE_LATENCY_SAMPLES));
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
      rnnoise,
      deepfilter,
      dpdfnet,
      sleep_delay,
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
    if let (true, Some(net)) = (gate_on, soft_net.as_mut()) {
      net.process(samples, |p| {
        *soft_prob = soft_prob.max(p);
        voice_soft |= p > *soft_voice_prob;
      });
    }
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
        sleep_delay.process(buf); // standing in: delayed to the heavy model's latency
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
      *rescue_prob = rescue.process(samples, stand);
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
      self.takeover = if t.done >= t.fade_at + TAKEOVER_FADE_SAMPLES {
        self.asleep = t.to_sleep;
        None
      } else {
        Some(t)
      };
    }
    self.quiet_samples = if voice || activity { 0 } else { self.quiet_samples.saturating_add(n) };
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

  /// Cold start: fresh network states. Allocates: only on user actions (denoiser switched back
  /// on, model change), never on a pause (`flush`).
  pub fn reset(&mut self) {
    self.reset_engines();
    self.in_net.reset();
    self.out_net = VoiceNet::new();
    if let Some(net) = self.soft_net.as_mut() {
      net.reset();
    }
    self.mute_left = 0;
    self.reset_timing();
  }

  /// `reset` for the real-time thread (the host paused the chain): drops all audio buffered from
  /// before, keeps the loaded models and their network states, allocates nothing.
  pub fn flush(&mut self) {
    (self.asleep, self.takeover, self.quiet_samples) = (false, None, 0);
    self.sleep_delay.clear();
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
    self.in_net.fifo.clear();
    self.out_net.fifo.clear();
    if let Some(net) = self.soft_net.as_mut() {
      net.flush();
    }
    self.reset_timing();
  }

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

  /// Start handing a pause to RNNoise (`to_sleep`) or speech back to the heavy model.
  fn start_takeover(&mut self, to_sleep: bool, latency: usize) {
    if to_sleep {
      self.rnnoise.restart();
      self.sleep_delay.set_len(latency.saturating_sub(RNNOISE_LATENCY_SAMPLES));
      self.sleep_delay.clear();
    } else {
      // Its FIFOs hold audio from before the pause; the model's own state warms up again.
      if let Some(df) = self.deepfilter.as_mut() {
        df.reset();
      }
      if let Some(dp) = self.dpdfnet.as_mut() {
        dp.restart();
      }
      self.in_net.fifo.clear();
    }
    let fade_at = if to_sleep { latency.max(STAND_IN_WARMUP_SAMPLES) } else { latency };
    self.takeover = Some(Takeover { to_sleep, done: 0, fade_at });
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

  fn reset_engines(&mut self) {
    (self.asleep, self.takeover, self.quiet_samples) = (false, None, 0);
    self.sleep_delay.clear();
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

impl RnNoiseEngine {
  fn new() -> Self {
    Self {
      denoiser: DenoiseState::new(),
      input_fifo: Vec::with_capacity(FRAME_SIZE * 4),
      output_fifo: primed_fifo(FRAME_SIZE),
      prev_dry: [0.0; FRAME_SIZE],
    }
  }

  /// `on_prob`: RNNoise's voice probability of each input frame it completes.
  fn process(&mut self, samples: &mut [f32], strength: f32, mut on_prob: impl FnMut(f32)) {
    self.input_fifo.extend_from_slice(samples);

    let mut temp_out = [0.0f32; FRAME_SIZE];
    let mut temp_in = [0.0f32; FRAME_SIZE];
    // Linear mix: the default 0.75 keeps 25 % dry, so RNNoise takes at most ~12 dB off any
    // noise. Kept on purpose. A dB mix (dry 6 dB down to 20 / 30 dB, `private_report`,
    // auto + lookahead, 2026-11-08) made pauses 19 / 25 dB quieter instead of 12-14, but the
    // soft syllables the gate cuts rose 1.6-2.4 -> 2.6-4.3 / 3.8-5.9 %. A few keys let through
    // beat one cut letter. A stricter output detector for RNNoise (0.3 -> 0.5: keys open the
    // gate 18 % instead of 31 %) cost +0.3-0.4 points too and was dropped as well.
    let (wet_gain, dry_gain) = (strength, 1.0 - strength);

    while self.input_fifo.len() >= FRAME_SIZE {
      for (dst, src) in temp_in.iter_mut().zip(self.input_fifo.drain(..FRAME_SIZE)) {
        *dst = src * I16_SCALE;
      }

      on_prob(self.denoiser.process_frame(&mut temp_out, &temp_in));

      for (wet, dry) in temp_out.iter().zip(self.prev_dry.iter()) {
        self.output_fifo.push((wet_gain * wet + dry_gain * dry) / I16_SCALE);
      }
      self.prev_dry = temp_in;
    }

    drain_fifo(&mut self.output_fifo, samples);
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
    self.input_fifo.clear();
    reprime_fifo(&mut self.output_fifo, FRAME_SIZE);
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
      hop,
      input_fifo: Vec::with_capacity(hop * 4),
      output_fifo: primed_fifo(hop),
      in_frame: Array2::zeros((1, hop)),
      out_frame: Array2::zeros((1, hop)),
      error_logged: false,
    })
  }

  fn process(&mut self, samples: &mut [f32], strength: f32, pre_gain: f32) {
    // Strength maps to the model's own attenuation limit (no dry/wet blend: DF's output
    // is delayed by its look-ahead, a blend would comb-filter).
    self.model.atten_lim = atten_lim(strength);

    self.input_fifo.extend(samples.iter().map(|s| s * pre_gain));
    while self.input_fifo.len() >= self.hop {
      for (dst, src) in self.in_frame.iter_mut().zip(self.input_fifo.drain(..self.hop)) {
        *dst = src;
      }
      match self.model.process(self.in_frame.view(), self.out_frame.view_mut()) {
        Ok(_) => self.output_fifo.extend(self.out_frame.iter().copied()),
        Err(e) => {
          if !self.error_logged {
            log::error!("[Denoise] DeepFilterNet frame failed, passing audio through: {e:#}");
            self.error_logged = true;
          }
          self.output_fifo.extend(self.in_frame.iter().copied());
        }
      }
    }
    drain_fifo(&mut self.output_fifo, samples);
    samples.iter_mut().for_each(|s| *s /= pre_gain);
  }

  fn reset(&mut self) {
    self.input_fifo.clear();
    reprime_fifo(&mut self.output_fifo, self.hop);
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
    Self { state, fifo: Vec::with_capacity(FRAME_SIZE * 4), frame: [0.0; FRAME_SIZE], scratch: [0.0; FRAME_SIZE] }
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
    self.fifo.extend_from_slice(samples);
    while self.fifo.len() >= FRAME_SIZE {
      for (dst, src) in self.frame.iter_mut().zip(self.fifo.drain(..FRAME_SIZE)) {
        *dst = src * I16_SCALE;
      }
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
    self.fifo.clear();
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

  /// `out`: DFN3 output, in place; `rnn`: RNNoise's output of the same samples. Returns the
  /// detector's highest voice probability in this block (-1: no frame completed).
  fn process(&mut self, out: &mut [f32], rnn: &[f32]) -> f32 {
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
      let cut = self.hold_left > 0 && self.env_rnn > self.env_dfn * ratio;
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
    self.net.fifo.clear();
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

/// One frame of silence keeps output length == input length for any block size.
pub(crate) fn primed_fifo(frame: usize) -> Vec<f32> {
  let mut fifo = Vec::with_capacity(frame * 4);
  fifo.resize(frame, 0.0);
  fifo
}

/// `primed_fifo` again, reusing the allocation (for resets on the real-time thread).
pub(crate) fn reprime_fifo(fifo: &mut Vec<f32>, frame: usize) {
  fifo.clear();
  fifo.resize(frame, 0.0);
}

/// Move processed samples out; the primed frame guarantees enough are available.
pub(crate) fn drain_fifo(fifo: &mut Vec<f32>, samples: &mut [f32]) {
  let drain_len = samples.len().min(fifo.len());
  for (dst, src) in samples.iter_mut().zip(fifo.drain(..drain_len)) {
    *dst = src;
  }
  // Only reachable if a caller block exceeds the priming; output silence, never raw input.
  for s in samples.iter_mut().skip(drain_len) {
    *s = 0.0;
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use gamersequalizer_common::biquad::BiquadCascade;
  use gamersequalizer_common::profile::{BiquadBand, FilterType};

  #[test]
  fn test_ai_denoiser_smoke() {
    let mut isolator = AiVoiceIsolator::new();
    let mut buffer = vec![0.05f32; 960]; // 2 frames
    isolator.process_samples(&mut buffer);
    assert_eq!(buffer.len(), 960);
  }

  #[test]
  fn test_odd_block_size_never_leaks_raw_input() {
    // Regression: with blocks that are not a multiple of 480, the unprocessed
    // tail used to be passed through raw, mixing dry and denoised audio.
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    let mut first = vec![0.5f32; 300];
    isolator.process_samples(&mut first);
    assert!(first.iter().all(|&s| s == 0.0), "first block must be primed silence");

    for _ in 0..20 {
      let mut block = vec![0.5f32; 441];
      isolator.process_samples(&mut block);
      assert_eq!(block.len(), 441);
      assert!(block.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }
  }

  #[test]
  fn test_bypass_passes_through_and_reenable_resets() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_strength(0.0);
    let mut block = vec![0.25f32; 480];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|&s| s == 0.25));

    isolator.set_strength(1.0);
    let mut block = vec![0.25f32; 480];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|&s| s == 0.0), "re-enable starts from primed silence");
  }

  #[test]
  fn test_bypass_still_hears_speech_for_the_leveler() {
    // Regression: with the denoiser off no detector ran, so the leveler never learned.
    let (clean, _) = speech_over_keyboard(-30.0, 0.0);
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_strength(0.0);
    let (mut speech_blocks, mut x) = (0, clean.clone());
    for b in x.chunks_mut(480) {
      isolator.process_samples(b);
      speech_blocks += usize::from(isolator.heard_voice());
    }
    assert_eq!(x, clean, "bypass leaves the audio untouched");
    assert_eq!(isolator.output_lead_samples(), 0);
    assert!(speech_blocks > clean.len() / 480 / 10, "heard speech in {speech_blocks} blocks");
  }

  #[test]
  fn test_debug_state_reports_the_detector_and_the_gate() {
    let (clean, _) = speech_over_keyboard(-30.0, 0.0);
    let mut isolator = AiVoiceIsolator::new();
    let mut x = clean.get(..480 * 20).unwrap_or_default().to_vec();
    let mut seen_soft = false;
    for b in x.chunks_mut(480) {
      isolator.process_samples(b);
      let d = isolator.debug_state();
      assert!((0.0..=1.0).contains(&d.in_prob), "RNNoise's own probability, one frame per block: {}", d.in_prob);
      assert!(d.gate_enabled && d.gate_gain <= 1.0 && d.rescue_prob == -1.0);
      seen_soft |= (0.0..=1.0).contains(&d.soft_prob);
      assert!(d.soft_prob == -1.0 || (0.0..=1.0).contains(&d.soft_prob), "{}", d.soft_prob);
    }
    assert!(seen_soft, "Silero completes a chunk every 32 ms");
  }

  #[test]
  fn test_in_net_rarely_fires_on_keys_alone() {
    // RNNoise 0.2 at `IN_NET_VOICE_PROB`: 0.5 % of frames in rnnoise2's `vad_sweep`.
    let probs = in_net_probs(&keyboard_only(-20.0));
    let share = probs.iter().filter(|&&p| p > IN_NET_VOICE_PROB).count() as f32 / probs.len().max(1) as f32;
    eprintln!("in_net, keys alone: {:.1} % of frames above {IN_NET_VOICE_PROB}", share * 100.0);
    assert!(share < 0.02, "{share}");
  }

  #[test]
  fn test_in_net_hears_speech_under_keys() {
    // 97 % in rnnoise2's `vad_sweep` (the 2017 detector at 0.5: 82 %).
    let (clean, mixed) = speech_over_keyboard(-20.0, 0.0);
    let probs = in_net_probs(&mixed);
    let speech_db = db(rms(&clean)) - 15.0;
    let (mut speech, mut heard) = (0usize, 0usize);
    // Probability t + 1 describes frame t (one frame of network lookahead).
    for (frame, p) in clean.as_chunks::<FRAME_SIZE>().0.iter().zip(probs.iter().skip(1)).skip(50) {
      if db(rms(frame)) >= speech_db {
        speech += 1;
        heard += usize::from(*p > IN_NET_VOICE_PROB);
      }
    }
    let share = heard as f32 / speech.max(1) as f32;
    eprintln!("in_net, speech over keys at 0 dB SNR: {:.1} % of {speech} speech frames heard", share * 100.0);
    assert!(speech > 100 && share > 0.9, "{heard} / {speech}");
  }

  #[test]
  fn test_deepfilter_loads_and_switches_both_ways() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    isolator.set_model(DenoiseModel::DeepFilterNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet, "load error: {:?}", isolator.df_error);

    let mut block = vec![0.1f32; 441];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));

    isolator.set_model(DenoiseModel::RnNoise);
    let mut block = vec![0.1f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  }

  #[test]
  fn test_heavy_model_sleeps_in_pauses_and_wakes_without_a_gap() {
    let (clean, _) = speech_over_keyboard(-30.0, 0.0);
    let hiss = |secs: usize| {
      let mut x = 1u32;
      (0..secs * 48_000)
        .map(|_| {
          x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
          ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 1e-3 // white noise, -66 dBFS
        })
        .collect::<Vec<f32>>()
    };
    let mut input = hiss(3);
    input.extend_from_slice(&clean);
    // 5 s: the clean speech has digital silence, which holds the noise floor down for 1.5 s.
    input.extend(hiss(5));
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let run = |sleep_after: usize| {
        let mut iso = AiVoiceIsolator::new();
        iso.set_model(model);
        iso.set_gate_threshold_db(GATE_OFF_DB - 1.0);
        iso.sleep_after = sleep_after;
        let (mut out, mut asleep) = (input.clone(), Vec::new());
        for block in out.chunks_mut(480) {
          iso.process_samples(block);
          asleep.push(iso.asleep());
        }
        (out, asleep)
      };
      let (out, asleep) = run(SLEEP_AFTER_SAMPLES);
      let (reference, _) = run(usize::MAX);
      assert!(asleep.get(299).copied().unwrap_or(false), "{model:?}: asleep after 3 s of hiss");
      assert!(asleep.last().copied().unwrap_or(false), "{model:?}: asleep again after the speech");
      let speech_blocks = (300..300 + clean.len() / 480).filter(|&b| !asleep[b]).count();
      assert!(speech_blocks > clean.len() / 480 / 2, "{model:?}: awake in {speech_blocks} speech blocks");
      // No gap: wherever the never-sleeping run is loud, the sleeping one is too.
      let frame_db = |x: &[f32]| 10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32 + 1e-20).log10();
      for (i, (o, r)) in out.chunks(480).zip(reference.chunks(480)).enumerate() {
        if frame_db(r) > -50.0 {
          assert!(
            frame_db(o) > frame_db(r) - 12.0,
            "{model:?} block {i}: {:.1} dB vs {:.1} dB",
            frame_db(o),
            frame_db(r)
          );
        }
      }
    }
  }

  #[test]
  fn test_live_switch_loads_off_thread_while_rnnoise_runs() {
    // Regression: a live switch loaded the model inside the audio callback (0.1-0.2 s stall).
    let mut isolator = AiVoiceIsolator::new();
    isolator.request_model(DenoiseModel::DeepFilterNet);
    let started = Instant::now();
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise, "RNNoise runs until the model arrives");
    assert!(started.elapsed().as_millis() < 50, "the block waited for the load: {:?}", started.elapsed());
    while isolator.active_model() != DenoiseModel::DeepFilterNet {
      assert!(started.elapsed().as_secs() < 20, "never took over, load error: {:?}", isolator.df_error);
      std::thread::sleep(std::time::Duration::from_millis(10));
      isolator.process_samples(&mut block);
      assert!(block.iter().all(|s| s.is_finite()));
    }
    // A second heavy model selected while none is loading: same path.
    isolator.request_model(DenoiseModel::DpdfNet);
    while isolator.active_model() != DenoiseModel::DpdfNet {
      assert!(started.elapsed().as_secs() < 40, "never took over, load error: {:?}", isolator.dpdf_error);
      std::thread::sleep(std::time::Duration::from_millis(10));
      isolator.process_samples(&mut block);
    }
  }

  #[test]
  fn test_overload_falls_back_until_reselected() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DeepFilterNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);

    isolator.fall_back_overloaded();
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);

    // Same model again (the per-block settings sync) must not undo the fallback...
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    // ...picking another model and back does.
    isolator.set_model(DenoiseModel::RnNoise);
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
    assert_eq!(isolator.limit(), DenoiseLimit::None);
  }

  /// Regression: picking the selected model again after a failed load changed nothing (the
  /// error was only cleared when the model id changed).
  #[test]
  fn test_retry_loads_a_model_that_failed_before() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.select(DenoiseModel::DeepFilterNet);
    isolator.store(load_failed(DenoiseModel::DeepFilterNet, "test".into()));
    assert_eq!(isolator.limit(), DenoiseLimit::LoadFailed);
    isolator.request_model(DenoiseModel::DeepFilterNet); // the per-block settings sync
    assert!(isolator.loading.is_none(), "a known failure is not retried every block");

    isolator.retry();
    assert!(isolator.loading.is_some(), "retry starts a load");
    assert_eq!(isolator.limit(), DenoiseLimit::None);
  }

  /// Regression: `pause()` rebuilt the RNNoise states on the audio thread (allocating). The
  /// flush must still drop everything from before the pause (DPDFNet's recurrent state holds
  /// 4 hops of it) and keep a loaded model.
  #[test]
  fn test_flush_drops_audio_from_before_and_keeps_the_model() {
    let (clean, _) = speech_over_keyboard(-20.0, 0.0);
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_gate_threshold_db(GATE_OFF_DB - 1.0); // gate off: everything left would be heard
      isolator.set_model(model);
      // Mid-speech: the loudest point to be cut off at.
      let mut speech = clean.clone();
      let mut loud_blocks = 0;
      for b in speech.chunks_mut(480) {
        loud_blocks += usize::from(rms(b) > 0.03);
        isolator.process_samples(b);
        if loud_blocks >= 30 {
          break;
        }
      }
      assert_eq!(isolator.active_model(), model, "load error: {:?} {:?}", isolator.df_error, isolator.dpdf_error);
      let engine = (
        isolator.deepfilter.as_deref().map(|e| e as *const DeepFilterEngine),
        isolator.dpdfnet.as_deref().map(|e| e as *const DpdfEngine),
      );
      isolator.flush();
      let mut silence = vec![0.0f32; 48_000];
      silence.chunks_mut(441).for_each(|b| isolator.process_samples(b));
      let peak = silence.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      assert!(peak < 1e-4, "{model:?}: audio from before the flush, peak {:.1} dBFS", db(peak));
      let after = (
        isolator.deepfilter.as_deref().map(|e| e as *const DeepFilterEngine),
        isolator.dpdfnet.as_deref().map(|e| e as *const DpdfEngine),
      );
      assert_eq!(after, engine, "{model:?}: same engine, not reloaded");
      assert!(isolator.loading.is_none());
      assert_eq!(isolator.active_model(), model);
    }
  }

  #[test]
  fn test_measured_latency_matches_constants() {
    // The APO feeds 480 (48 kHz device) or 96 (rate bridge) frames; 441 / 144 stand for hosts
    // and low-latency engine periods that don't divide a 10 ms hop. The gate and the leveler
    // trust the constant, so it must hold for every block size (DPDFNet ran 10 ms early at 480).
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      for block in [441, 480, 96, 144] {
        let measured = measure_latency_samples(model, -80.0, block);
        let expected = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
        eprintln!(
          "{model:?}, {block}-frame blocks: measured latency {measured} samples ({:.1} ms)",
          measured as f32 / 48.0
        );
        // Even at minimal strength the heavy models still output ~half wet signal; DPDFNet's
        // filtering shifts that half's phase by a few samples.
        let tolerance = if model == DenoiseModel::DpdfNet { 4 } else { 2 };
        assert!(
          measured.abs_diff(expected) <= tolerance,
          "{model:?}, {block}-frame blocks: measured {measured} samples, constant says {expected}"
        );
      }
    }
  }

  #[test]
  fn test_lookahead_adds_its_delay_only_with_the_gate_on() {
    let model = DenoiseModel::RnNoise;
    let base = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
    let ahead = measure_latency_samples(model, GATE_AUTO_LOOKAHEAD, 441);
    assert!(ahead.abs_diff(base + lookahead_samples(model)) <= 2, "{ahead} vs {base} + lookahead");
    // Off gate: lookahead would only add latency, so it does not run.
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    isolator.set_gate_threshold_db(-80.0);
    let mut block = vec![0.1f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.delay.len(), 0);
  }

  #[test]
  fn test_lookahead_keeps_more_word_onsets() {
    // Onsets after pauses under loud keys are what the gate still cut (see RNNOISE_LOOKAHEAD_SAMPLES).
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
      let auto = run_quality(model, GATE_AUTO, &clean, &mixed);
      let ahead = run_quality(model, GATE_AUTO_LOOKAHEAD, &clean, &mixed);
      eprintln!("{model:?}: auto {auto:?}, lookahead {ahead:?}");
      assert!(ahead.soft_dropouts <= auto.soft_dropouts, "{model:?}: {ahead:?} vs {auto:?}");
      assert!(ahead.dropouts <= auto.dropouts, "{model:?}: {ahead:?} vs {auto:?}");
    }
  }

  #[test]
  fn test_delay_line_delays_across_blocks() {
    let mut delay = DelayLine::new();
    delay.set_len(3);
    let mut a = [1.0f32, 2.0];
    let mut b = [3.0f32, 4.0, 5.0];
    delay.process(&mut a);
    delay.process(&mut b);
    assert_eq!((a, b), ([0.0, 0.0], [0.0, 1.0, 2.0]));
  }

  #[test]
  fn test_dpdfnet_loads_and_switches_both_ways() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    isolator.set_model(DenoiseModel::DpdfNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DpdfNet, "load error: {:?}", isolator.dpdf_error);
    let mut block = vec![0.1f32; 441];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
    isolator.set_model(DenoiseModel::RnNoise);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  }

  #[test]
  fn test_dpdfnet_overload_falls_back_to_rnnoise() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DpdfNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    isolator.fall_back_overloaded();
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);
  }

  #[test]
  fn test_dpdfnet_wet_and_dry_paths_are_aligned() {
    // The strength blend mixes the input back in; if its delay differs from the model's, the
    // two comb-filter. The dry path is checked by the latency test (strength ~0); here the
    // fully wet output of near-clean speech must match the speech best at the same delay,
    // clearly better than one hop (10 ms) earlier or later.
    let (clean, mixed) = speech_over_keyboard(-25.0, 20.0);
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DpdfNet);
    isolator.set_strength(1.0);
    isolator.set_gate_threshold_db(GATE_OFF_DB - 1.0);
    let mut out = mixed.clone();
    for block in out.chunks_mut(441) {
      isolator.process_samples(block);
    }
    let corr = |lag: usize| -> f32 {
      let num: f32 = clean.iter().zip(out.iter().skip(lag)).map(|(a, b)| a * b).sum();
      let den = (clean.iter().map(|a| a * a).sum::<f32>() * out.iter().skip(lag).map(|b| b * b).sum::<f32>()).sqrt();
      num / den.max(1e-12)
    };
    let expected = (AiVoiceIsolator::latency_ms(DenoiseModel::DpdfNet) * 48.0) as usize;
    let best_near = |center: usize| {
      (center - 24..=center + 24).map(|l| (l, corr(l))).fold((0, f32::MIN), |b, c| if c.1 > b.1 { c } else { b })
    };
    let (at, here) = best_near(expected);
    let (_, early) = best_near(expected - 480);
    let (_, late) = best_near(expected + 480);
    eprintln!("wet correlation: {here:.3} at lag {at} (expected {expected}), one hop early {early:.3}, late {late:.3}");
    assert!(at.abs_diff(expected) <= 4, "best lag {at}, expected {expected}");
    assert!(here > 0.8 && here > early + 0.2 && here > late + 0.2, "{here} vs {early} / {late}");
  }

  #[test]
  fn test_dpdfnet_keeps_soft_speech_over_keyboard() {
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("loud keys, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
    assert!(dpdf.dropouts < 0.01 && dpdf.soft_dropouts < 0.10, "{dpdf:?}");
    assert!(dpdf.noise_suppression_db > 35.0, "{dpdf:?}");
    let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
    let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("quiet voice, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
    assert!(dpdf.soft_dropouts <= dfn.soft_dropouts, "{dpdf:?} vs DFN3 {dfn:?}");
  }

  #[test]
  fn test_deepfilter_keeps_soft_speech_over_keyboard() {
    // Regression: upstream min_db_thresh (-10 dB) zeroed 26 % of soft syllables here.
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    let q = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("DFN, loud keys: {q:?}");
    assert!(q.dropouts < 0.01 && q.soft_dropouts < 0.10, "{q:?}");
    // Keys 10 dB over the voice: next to words `ClickRescue` lets them through with the voice it
    // fills in (all pauses ~20 dB, measured 35+ without it); pauses away from speech stay clean.
    assert!(q.far_suppression_db > 35.0 && q.noise_suppression_db > 15.0, "{q:?}");
  }

  #[test]
  fn test_gate_keeps_loud_and_quiet_speech() {
    // Regression: the old level gate at its default -45 dB cut 13 % of a quiet voice
    // (-45 dBFS) and half of its soft syllables. The automatic gate has no level setting:
    // a loud (daytime) and a quiet (evening) voice must both pass like with no gate.
    for level in [-20.0f32, -45.0] {
      let (clean, mixed) = speech_over_keyboard(level, 0.0);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
        let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
        eprintln!("{model:?} voice {level} dBFS: gate {with_gate:?}, no gate {no_gate:?}");
        assert!(with_gate.dropouts <= no_gate.dropouts + 0.01, "{model:?} {level}: {with_gate:?} vs {no_gate:?}");
        assert!(
          with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.03,
          "{model:?} {level}: {with_gate:?} vs {no_gate:?}"
        );
      }
    }
  }

  #[test]
  fn test_gate_keeps_soft_syllables_under_loud_keys() {
    // Regression: with one 0.5 threshold the gate closed on soft syllables between louder
    // ones (+3.5 % cut for DFN3, +5.3 % for RNNoise over no gate). Hysteresis keeps them.
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
      let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
      eprintln!("{model:?} loud keys: gate {with_gate:?}, no gate {no_gate:?}");
      assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.015, "{model:?}: {with_gate:?} vs {no_gate:?}");
    }
  }

  #[test]
  fn test_gate_hold_covers_model_latency() {
    // Regression: the hold ran from when the detector heard the raw input, a model latency
    // before that audio leaves the model, so DPDFNet (60 ms) lost 1.9 -> 3.1 % of a quiet
    // voice's soft syllables to the gate. Now 2.5 % (DFN3 5.3 -> 5.6 %); the rest are onsets
    // the detector recognises late (a 400 ms hold changes nothing).
    let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
      let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
      eprintln!("{model:?} quiet voice: gate {with_gate:?}, no gate {no_gate:?}");
      assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.008, "{model:?}: {with_gate:?} vs {no_gate:?}");
    }
  }

  #[test]
  fn test_gate_shuts_on_typing_without_speech() {
    // Max open share per model (DFN3, DPDFNet, RNNoise). The gate favours the voice: the output
    // detector also hears keys a model leaves in, so RNNoise (keys still audible in its output)
    // opens on a third of loud typing (measured 31 %, 11 % at -40 dBFS); DFN3 5 %, DPDFNet 2 %.
    // Closed or not, keys stay `GATE_FLOOR_DB` or more under the model's output.
    for (level, max_open) in [(-20.0f32, [0.08f32, 0.03, 0.40]), (-40.0, [0.0, 0.0, 0.15])] {
      let keys = keyboard_only(level);
      for (model, max_open) in
        [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise].into_iter().zip(max_open)
      {
        let open = open_share(&run(model, GATE_AUTO, &keys), &run(model, GATE_OFF_DB - 1.0, &keys));
        eprintln!("{model:?} keys only {level} dBFS: gate open {:.1} % of the time", open * 100.0);
        assert!(open <= max_open, "{model:?} {level}: open {open}");
      }
    }
  }

  #[test]
  fn test_soft_key_keeps_more_speech_and_no_more_keys() {
    // The Silero key only adds open time where it hears a voice: no more speech lost than
    // without it, and typing alone opens the gate as rarely as before (it never reaches the
    // threshold on keys). The shared fixture shows no gain (soft syllables 0.9 % either way);
    // the private voice does, see `SOFT_VOICE_PROB`.
    let without_soft = |input: &[f32]| {
      let mut isolator = AiVoiceIsolator::new();
      isolator.soft_voice_prob = f32::INFINITY;
      let mut out = input.to_vec();
      out.chunks_mut(480).for_each(|block| isolator.process_samples(block));
      out
    };
    let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
    let latency = latency_samples(DenoiseModel::RnNoise);
    let with = measure(&clean, &mixed, &run(DenoiseModel::RnNoise, GATE_AUTO, &mixed), latency);
    let without = measure(&clean, &mixed, &without_soft(&mixed), latency);
    eprintln!("RNNoise quiet voice: with the soft key {with:?}, without {without:?}");
    assert!(with.soft_dropouts <= without.soft_dropouts && with.dropouts <= without.dropouts, "{with:?} vs {without:?}");
    let keys = keyboard_only(-20.0);
    let ungated = run(DenoiseModel::RnNoise, GATE_OFF_DB - 1.0, &keys);
    let (open, open_without) =
      (open_share(&run(DenoiseModel::RnNoise, GATE_AUTO, &keys), &ungated), open_share(&without_soft(&keys), &ungated));
    eprintln!(
      "RNNoise keys only: gate open {:.1} % with the soft key, {:.1} % without",
      open * 100.0,
      open_without * 100.0
    );
    assert!(open <= open_without + 0.01, "{open} vs {open_without}");
  }

  #[test]
  fn test_closed_gate_attenuates_by_floor_not_mute() {
    let mut gate = NoiseGate::new(true);
    let mut block = vec![1.0f32; 48_000];
    gate.process(&mut block, 0);
    let last = block.last().copied().unwrap_or_default();
    assert!((db(last) - GATE_FLOOR_DB).abs() < 0.1, "{} dB", db(last));
  }

  #[test]
  fn test_gate_floor_follows_leveler_boost_within_limits() {
    let closed_db = |extra: f32| {
      let mut iso = AiVoiceIsolator::new();
      iso.set_gate_floor_extra_db(extra);
      let mut block = vec![1.0f32; 48_000];
      iso.gate.process(&mut block, 0);
      db(block.last().copied().unwrap_or_default())
    };
    assert!((closed_db(6.0) - (GATE_FLOOR_DB - 6.0)).abs() < 0.1);
    // A leveler cut never lifts the floor; the boost counts up to the limit only.
    assert!((closed_db(-10.0) - GATE_FLOOR_DB).abs() < 0.1);
    assert!((closed_db(24.0) - (GATE_FLOOR_DB - GATE_FLOOR_EXTRA_MAX_DB)).abs() < 0.1);
    assert!((closed_db(f32::NAN) - GATE_FLOOR_DB).abs() < 0.1);
  }

  /// Share of 10 ms frames (after a 1 s warm-up) in which the gate is open: the gated output
  /// is within 6 dB of the same model's ungated output (closed = `GATE_FLOOR_DB` under it).
  fn open_share(gated: &[f32], ungated: &[f32]) -> f32 {
    let frames: Vec<bool> = gated
      .as_chunks::<480>()
      .0
      .iter()
      .zip(ungated.as_chunks::<480>().0)
      .skip(100)
      .map(|(g, u)| rms(u) > 1e-6 && db(rms(g)) > db(rms(u)) - 6.0)
      .collect();
    frames.iter().filter(|&&o| o).count() as f32 / frames.len().max(1) as f32
  }

  /// What the UI writes for "Auto" / "Auto" with "Protect word onsets".
  const GATE_AUTO: f32 = 0.0;
  const GATE_AUTO_LOOKAHEAD: f32 = 1.0;

  /// Default strength, 10 ms blocks like the audio engine.
  fn run(model: DenoiseModel, gate_db: f32, input: &[f32]) -> Vec<f32> {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(gate_db);
    let mut out = input.to_vec();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    assert_eq!(isolator.active_model(), model);
    out
  }

  fn run_quality(model: DenoiseModel, gate_db: f32, clean: &[f32], mixed: &[f32]) -> Quality {
    let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
    let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize + lookahead;
    measure(clean, mixed, &run(model, gate_db, mixed), latency)
  }

  const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/denoise");
  const KEYBOARD_CLIPS: [&str; 6] = [
    "1-137-A-32.wav",
    "1-53501-A-32.wav",
    "1-118206-A-31.wav",
    "1-61212-A-32.wav",
    "1-18655-A-31.wav",
    "1-79711-A-32.wav",
  ];

  #[derive(Debug)]
  struct Quality {
    /// Share of loud speech frames the output lost (more than 15 dB below the clean speech).
    dropouts: f32,
    /// Same as `dropouts` for soft speech (syllable onsets/tails, up to 15 dB below average).
    soft_dropouts: f32,
    /// How much quieter the output is than the input in speech pauses (keyboard only), dB.
    noise_suppression_db: f32,
    /// Same, only pauses at least `FAR_PAUSE_FRAMES` from speech: the gaps right next to words
    /// get keys through where DFN3's `ClickRescue` fills in a cut voice.
    far_suppression_db: f32,
  }

  /// 150 ms: the rescue's hold (100 ms), crossfade out (30 ms) and its detector's frame.
  const FAR_PAUSE_FRAMES: usize = 15;

  /// Clean speech at `speech_rms_db` dBFS and the same speech with mechanical keyboard and
  /// mouse clicks mixed in at `snr_db`. Both at 48 kHz.
  fn speech_over_keyboard(speech_rms_db: f32, snr_db: f32) -> (Vec<f32>, Vec<f32>) {
    mix("speech_clean.wav", &KEYBOARD_CLIPS, None, speech_rms_db, snr_db)
  }

  /// `speech_over_keyboard` for any fixtures (paths relative to `FIXTURES`). `low_cut_hz`
  /// high-passes the speech first (24 dB/oct, like the mic EQ's low cut): breath pops and
  /// desk bumps in a raw recording are loud but not speech, and would count as lost speech.
  fn mix(
    speech: &str,
    noise_clips: &[&str],
    low_cut_hz: Option<f32>,
    speech_rms_db: f32,
    snr_db: f32,
  ) -> (Vec<f32>, Vec<f32>) {
    let mut clean = read_wav_48k(speech);
    if let Some(frequency) = low_cut_hz {
      let mut eq = BiquadCascade::default();
      let band =
        BiquadBand { filter_type: FilterType::HighPass, frequency, q: 0.707, slope_db: 24, ..BiquadBand::default() };
      eq.set_bands(&[band], 48_000.0);
      eq.process(&mut clean);
    }
    let mut noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
    while noise.len() < clean.len() {
      noise.extend_from_within(..);
    }
    noise.truncate(clean.len());
    let clean = scale_to_db(&clean, speech_rms_db);
    let noise = scale_to_db(&noise, speech_rms_db - snr_db);
    let mixed = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
    (clean, mixed)
  }

  /// Keyboard and mouse clicks only (no speech) at `rms_db` dBFS, 48 kHz.
  fn keyboard_only(rms_db: f32) -> Vec<f32> {
    noise_only(&KEYBOARD_CLIPS, rms_db)
  }

  fn noise_only(noise_clips: &[&str], rms_db: f32) -> Vec<f32> {
    let noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
    scale_to_db(&noise, rms_db)
  }

  /// Compares the output (delayed by the model's latency) with the clean speech, per 10 ms frame.
  fn measure(clean: &[f32], mixed: &[f32], out: &[f32], latency: usize) -> Quality {
    const FRAME: usize = 480;
    const WARMUP_FRAMES: usize = 50;
    let delayed = &out[latency..];
    let clean_db = db(rms(clean));
    let (mut loud, mut lost, mut soft, mut soft_lost) = (0usize, 0usize, 0usize, 0usize);
    let (mut pause_in, mut pause_out, mut far_in, mut far_out) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let near_speech: Vec<bool> = clean.as_chunks::<FRAME>().0.iter().map(|c| db(rms(c)) >= clean_db - 40.0).collect();
    for (i, ((c, m), o)) in clean
      .as_chunks::<FRAME>()
      .0
      .iter()
      .zip(mixed.as_chunks::<FRAME>().0)
      .zip(delayed.as_chunks::<FRAME>().0)
      .enumerate()
      .skip(WARMUP_FRAMES)
    {
      let c_db = db(rms(c));
      let window = i.saturating_sub(FAR_PAUSE_FRAMES)..(i + FAR_PAUSE_FRAMES + 1).min(near_speech.len());
      if c_db < clean_db - 40.0 && !near_speech.get(window).unwrap_or_default().iter().any(|&s| s) {
        far_in += m.iter().map(|x| x * x).sum::<f32>();
        far_out += o.iter().map(|x| x * x).sum::<f32>();
      }
      let is_lost = db(rms(o)) - c_db < -15.0;
      if c_db >= clean_db {
        loud += 1;
        lost += is_lost as usize;
      } else if c_db >= clean_db - 15.0 {
        soft += 1;
        soft_lost += is_lost as usize;
      } else if c_db < clean_db - 40.0 {
        pause_in += m.iter().map(|x| x * x).sum::<f32>();
        pause_out += o.iter().map(|x| x * x).sum::<f32>();
      }
    }
    Quality {
      dropouts: lost as f32 / loud.max(1) as f32,
      soft_dropouts: soft_lost as f32 / soft.max(1) as f32,
      noise_suppression_db: 10.0 * (pause_in.max(1e-12) / pause_out.max(1e-12)).log10(),
      far_suppression_db: 10.0 * (far_in.max(1e-12) / far_out.max(1e-12)).log10(),
    }
  }

  /// WAV fixture as f32 (first channel, like the APO), linearly resampled to 48 kHz (good
  /// enough for test noise).
  pub(crate) fn read_wav_48k(name: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(format!("{FIXTURES}/{name}")).unwrap();
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let samples: Vec<f32> = match spec.sample_format {
      hound::SampleFormat::Int => {
        let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        reader.samples::<i32>().step_by(channels).map(|s| s.unwrap() as f32 / scale).collect()
      }
      hound::SampleFormat::Float => reader.samples::<f32>().step_by(channels).map(|s| s.unwrap()).collect(),
    };
    if spec.sample_rate == 48_000 {
      return samples;
    }
    let step = spec.sample_rate as f64 / 48_000.0;
    let len = ((samples.len() - 1) as f64 / step) as usize;
    (0..len)
      .map(|i| {
        let pos = i as f64 * step;
        let (j, frac) = (pos as usize, (pos.fract()) as f32);
        samples[j] * (1.0 - frac) + samples[j + 1] * frac
      })
      .collect()
  }

  fn scale_to_db(x: &[f32], target_db: f32) -> Vec<f32> {
    let gain = 10f32.powf(target_db / 20.0) / rms(x).max(1e-9);
    x.iter().map(|s| s * gain).collect()
  }

  /// `in_net`'s voice probability per 480-sample frame of `x`.
  fn in_net_probs(x: &[f32]) -> Vec<f32> {
    let mut net = VoiceNet::new_input();
    assert!(matches!(net.state, VoiceModel::New(_)), "RNNoise 0.2 weights must load");
    let mut probs = Vec::with_capacity(x.len() / FRAME_SIZE);
    for block in x.chunks(FRAME_SIZE) {
      net.process(block, |p| probs.push(p));
    }
    probs
  }

  /// Regression (2026-11-09): DeepFilterNet cut a voice quieter than about -52 dBFS by 30-45 dB
  /// (studio speech at -58 dBFS: -43 dB); the leveler's boost as pre-gain brings it back.
  #[test]
  fn test_deepfilter_keeps_a_quiet_voice_with_pre_gain() {
    let quiet = scale_to_db(&read_wav_48k("speech_clean.wav"), -58.0);
    let kept_db = |pre_gain_db: f32| {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(DenoiseModel::DeepFilterNet);
      isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
      isolator.set_pre_gain_db(pre_gain_db);
      let mut out = quiet.clone();
      for block in out.chunks_mut(480) {
        isolator.process_samples(block);
      }
      let lead = latency_samples(DenoiseModel::DeepFilterNet) + lookahead_samples(DenoiseModel::DeepFilterNet);
      db(rms(&out[lead..])) - db(rms(&quiet[..quiet.len() - lead]))
    };
    assert!(kept_db(24.0) > -6.0, "voice changed by {:.1} dB with the pre-gain", kept_db(24.0));
    assert!(kept_db(f32::NAN) == kept_db(0.0), "NaN counts as no gain");
  }

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
  }

  fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
  }

  /// Runs noise through the model at minimal strength (mostly dry, so the signal survives) in
  /// odd-sized blocks and finds the input->output lag by cross-correlation.
  fn measure_latency_samples(model: DenoiseModel, gate_db: f32, block: usize) -> usize {
    const LEN: usize = 48_000;
    const MAX_LAG: usize = 3_000;
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_strength(0.01);
    isolator.set_gate_threshold_db(gate_db);
    let mut x = 12_345u32;
    let input: Vec<f32> = (0..LEN)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
      })
      .collect();
    let mut output = input.clone();
    for chunk in output.chunks_mut(block) {
      isolator.process_samples(chunk);
    }
    assert_eq!(isolator.active_model(), model);
    const SKIP: usize = 6_000; // model warm-up; must be >= MAX_LAG
    let window = LEN - SKIP;
    (0..MAX_LAG)
      .map(|lag| {
        let dry = input.iter().skip(SKIP - lag);
        let c: f32 = dry.zip(output.iter().skip(SKIP)).take(window).map(|(a, b)| a * b).sum();
        (lag, c)
      })
      .fold((0, f32::MIN), |best, cur| if cur.1 > best.1 { cur } else { best })
      .0
  }

  /// Report on the developer's own recordings in the git-ignored `test_data/private/`
  /// (`speech.wav`, `keys.wav`: mono WAV). Prints gate vs no gate per model, no thresholds:
  /// a private voice can't back a shared regression test. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release private_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn private_report() {
    const SPEECH: &str = "../private/speech.wav";
    const KEYS: [&str; 1] = ["../private/keys.wav"];
    if [SPEECH, KEYS[0]].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
      eprintln!("skipped: put speech.wav and keys.wav into test_data/private/");
      return;
    }
    eprintln!("soft% / loud% lost [pause suppression dB], gate auto | auto + lookahead | no gate");
    for (level, snr) in [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)] {
      let (clean, mixed) = mix(SPEECH, &KEYS, Some(100.0), level, snr);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let row = |q: Quality| {
          format!("{:4.1} / {:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
        };
        let gate = row(run_quality(model, GATE_AUTO, &clean, &mixed));
        let ahead = row(run_quality(model, GATE_AUTO_LOOKAHEAD, &clean, &mixed));
        let no_gate = row(run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed));
        eprintln!("voice {level:>4} dBFS, SNR {snr:>4} dB, {model:<13?} {gate} | {ahead} | {no_gate}");
      }
    }
    for level in [-20.0f32, -40.0] {
      let keys = noise_only(&KEYS, level);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let ungated = run(model, GATE_OFF_DB - 1.0, &keys);
        let open = open_share(&run(model, GATE_AUTO, &keys), &ungated);
        let ahead = run(model, GATE_AUTO_LOOKAHEAD, &keys);
        let open_ahead = open_share(ahead.get(lookahead_samples(model)..).unwrap_or_default(), &ungated);
        eprintln!(
          "keys only {level} dBFS, {model:<13?} gate open {:.1} %, with lookahead {:.1} %",
          open * 100.0,
          open_ahead * 100.0
        );
      }
    }
  }

  #[test]
  fn test_deepfilter_keeps_speech_under_hammered_mouse_clicks() {
    // Regression: a mouse button hammered next to the mic (peaks -3 dBFS over a -30 dBFS voice)
    // made DFN3 cut the voice with each click: > 15 dB lost in 16 % of the frames with a click
    // (private voice), 5.5 % here. `ClickRescue` fills those in from RNNoise: 5.8 % / 1.4 %.
    let (clean, clicks, mixed) = speech_with_clicks("speech_clean.wav", -3.0);
    let with = run_clicks(DenoiseModel::DeepFilterNet, 0.75, GATE_AUTO_LOOKAHEAD, false, &clean, &clicks, &mixed);
    let without = run_clicks(DenoiseModel::DeepFilterNet, 0.75, GATE_AUTO_LOOKAHEAD, true, &clean, &clicks, &mixed);
    eprintln!("DFN3 hammered clicks: rescue {with:?}, without {without:?}");
    assert!(with.lost15 < 0.06 && with.lost15 < without.lost15 / 2.0, "{with:?} vs {without:?}");
    assert!(with.quality.soft_dropouts <= without.quality.soft_dropouts, "{with:?} vs {without:?}");
    // Pauses away from speech stay as clean: the rescue is armed only by a voice in DFN3's output.
    assert!(with.quality.far_suppression_db > without.quality.far_suppression_db - 3.0, "{with:?} vs {without:?}");
  }

  /// Mouse clicks right next to the mic over speech, per model / strength / gate: soft% / loud%
  /// lost [pause suppression dB], and the speech frames with a click (click within 10 dB of the
  /// voice there): mean dB under the clean speech, shares lost > 6 / > 15 dB. Private voice
  /// when present. DFN3 also without `ClickRescue`.
  /// `cargo test -p gamersequalizer-mic-vst --release click_report -- --ignored --nocapture`
  #[test]
  #[ignore = "report, slow"]
  fn click_report() {
    const SPEECH: &str = "../private/speech.wav";
    let speech = if std::path::Path::new(FIXTURES).join(SPEECH).exists() { SPEECH } else { "speech_clean.wav" };
    eprintln!("speech: {speech}");
    for peak_db in [-20.0f32, -10.0, -3.0] {
      let (clean, clicks, mixed) = speech_with_clicks(speech, peak_db);
      for (model, strength, no_rescue) in [
        (DenoiseModel::DeepFilterNet, 0.75, false),
        (DenoiseModel::DeepFilterNet, 0.75, true),
        (DenoiseModel::DeepFilterNet, 0.25, true),
        (DenoiseModel::DpdfNet, 0.75, false),
        (DenoiseModel::RnNoise, 0.75, false),
      ] {
        for gate in [GATE_OFF_DB - 1.0, GATE_AUTO_LOOKAHEAD] {
          let c = run_clicks(model, strength, gate, no_rescue, &clean, &clicks, &mixed);
          eprintln!(
            "clicks {peak_db:>5} dBFS, {model:<13?}{} strength {strength:.2} gate {:<4} | {:4.1} / {:3.1} [{:4.1}] | clicks: {:4.1} dB, >6 {:4.1} %, >15 {:4.1} %",
            if no_rescue { " no rescue" } else { "          " },
            if gate > GATE_OFF_DB { "auto" } else { "off" },
            c.quality.soft_dropouts * 100.0,
            c.quality.dropouts * 100.0,
            c.quality.noise_suppression_db,
            c.mean_loss_db,
            c.lost6 * 100.0,
            c.lost15 * 100.0
          );
        }
      }
    }
  }

  /// A raw take of speech with keys hammered into it (git-ignored `test_data/private/keyboard_call.wav`,
  /// no clean reference). Per model (gate auto + lookahead, default strength) writes the processed take
  /// to `test_data/private/out/` for listening and counts speech frames DFN3 cut: 10 ms frames where
  /// RNNoise (which keeps a voice under keys) is speech-loud and the model's output > 10 dB under it.
  /// `cargo test -p gamersequalizer-mic-vst --release keyboard_call_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn keyboard_call_report() {
    const TAKE: &str = "../private/keyboard_call.wav";
    if !std::path::Path::new(FIXTURES).join(TAKE).exists() {
      eprintln!("skipped: put keyboard_call.wav into test_data/private/");
      return;
    }
    let input = read_wav_48k(TAKE);
    let out_dir = std::path::Path::new(FIXTURES).join("../private/out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let process = |model: DenoiseModel, no_rescue: bool| {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(model);
      isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
      isolator.no_rescue = no_rescue;
      let mut out = input.clone();
      for block in out.chunks_mut(480) {
        isolator.process_samples(block);
      }
      out.drain(..latency_samples(model) + lookahead_samples(model)); // aligned with the input
      out
    };
    let reference = process(DenoiseModel::RnNoise, false);
    let ref_db: Vec<f32> = reference.as_chunks::<480>().0.iter().map(|f| db(rms(f))).collect();
    let loud = ref_db.iter().copied().fold(f32::MIN, f32::max);
    for (name, model, no_rescue) in [
      ("dfn3_rescue", DenoiseModel::DeepFilterNet, false),
      ("dfn3_no_rescue", DenoiseModel::DeepFilterNet, true),
      ("dpdfnet", DenoiseModel::DpdfNet, false),
      ("rnnoise", DenoiseModel::RnNoise, false),
    ] {
      let out = process(model, no_rescue);
      let (mut speech, mut cut) = (0usize, 0usize);
      for (f, &r) in out.as_chunks::<480>().0.iter().zip(&ref_db).skip(100) {
        if r > loud - 25.0 {
          speech += 1;
          cut += (db(rms(f)) < r - 10.0) as usize;
        }
      }
      let quiet: Vec<f32> =
        out.as_chunks::<480>().0.iter().zip(&ref_db).filter(|(_, &r)| r < loud - 40.0).map(|(f, _)| rms(f)).collect();
      eprintln!(
        "{name:<15} speech frames cut > 10 dB under RNNoise: {:4.1} % of {speech}; quiet frames {:5.1} dBFS",
        cut as f32 * 100.0 / speech.max(1) as f32,
        db((quiet.iter().map(|x| x * x).sum::<f32>() / quiet.len().max(1) as f32).sqrt())
      );
      let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
      };
      let mut w = hound::WavWriter::create(out_dir.join(format!("keyboard_call_{name}.wav")), spec).unwrap();
      out.iter().for_each(|&s| w.write_sample(s).unwrap());
      w.finalize().unwrap();
    }
  }

  /// Default strength candidates per model (gate auto + lookahead): soft% / loud% lost
  /// [pause suppression dB, far pauses dB] on the keyboard mixes (private when present) and
  /// hammered clicks at -3 dBFS (+ frames lost > 15 dB under the clicks).
  /// `cargo test -p gamersequalizer-mic-vst --release strength_report -- --ignored --nocapture`
  #[test]
  #[ignore = "report, slow"]
  fn strength_report() {
    const SPEECH: &str = "../private/speech.wav";
    const KEYS: [&str; 1] = ["../private/keys.wav"];
    let private = [SPEECH, KEYS[0]].iter().all(|f| std::path::Path::new(FIXTURES).join(f).exists());
    let (speech, keys): (&str, &[&str]) = if private { (SPEECH, &KEYS) } else { ("speech_clean.wav", &KEYBOARD_CLIPS) };
    eprintln!(
      "{} recordings; mixes voice/SNR -20/0, -45/0, -30/-10 dB, then clicks -3 dBFS",
      if private { "private" } else { "shared" }
    );
    let mixes: Vec<_> = [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)]
      .iter()
      .map(|&(l, s)| mix(speech, keys, Some(100.0), l, s))
      .collect();
    let (cc, ck, cm) = speech_with_clicks(speech, -3.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      for strength in [0.75f32, 0.9, 1.0] {
        let cells: Vec<String> = mixes
          .iter()
          .map(|(clean, mixed)| {
            let mut iso = AiVoiceIsolator::new();
            iso.set_model(model);
            iso.set_strength(strength);
            iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
            let mut out = mixed.clone();
            for b in out.chunks_mut(480) {
              iso.process_samples(b);
            }
            let q = measure(clean, mixed, &out, latency_samples(model) + lookahead_samples(model));
            format!(
              "{:4.1} / {:3.1} [{:4.1} {:4.1}]",
              q.soft_dropouts * 100.0,
              q.dropouts * 100.0,
              q.noise_suppression_db,
              q.far_suppression_db
            )
          })
          .collect();
        let c = run_clicks(model, strength, GATE_AUTO_LOOKAHEAD, false, &cc, &ck, &cm);
        eprintln!(
          "{model:<13?} {strength:.2} | {} | clicks {:4.1} / {:3.1} [{:4.1} {:4.1}] >15 {:4.1} %",
          cells.join(" | "),
          c.quality.soft_dropouts * 100.0,
          c.quality.dropouts * 100.0,
          c.quality.noise_suppression_db,
          c.quality.far_suppression_db,
          c.lost15 * 100.0
        );
      }
    }
  }

  /// Where keys leak next to words: the private voice at -30 dBFS under the private keys 10 dB
  /// louder, per model. Speech frames lost > 6 / > 15 dB, word tails (15-30 dB under the average)
  /// lost > 6 dB, then key suppression in pause frames by their distance from speech (10 ms
  /// frames after the last / before the next speech frame).
  /// `cargo test -p gamersequalizer-mic-vst --release near_pause_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn near_pause_report() {
    const SPEECH: &str = "../private/speech.wav";
    const KEYS: [&str; 1] = ["../private/keys.wav"];
    const BUCKETS: [(usize, usize); 5] = [(1, 1), (2, 2), (3, 5), (6, 10), (11, 1000)];
    if [SPEECH, KEYS[0]].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
      eprintln!("skipped: put speech.wav and keys.wav into test_data/private/");
      return;
    }
    let (clean, mixed) = mix(SPEECH, &KEYS, Some(100.0), -30.0, -10.0);
    let clean_db = db(rms(&clean));
    let speech: Vec<bool> = clean.as_chunks::<480>().0.iter().map(|c| db(rms(c)) >= clean_db - 40.0).collect();
    // Frames to the nearest speech frame behind / ahead of each frame.
    let distances = |frames: &mut dyn Iterator<Item = usize>| {
      let (mut d, mut out) = (usize::MAX / 2, vec![0usize; speech.len()]);
      for i in frames {
        d = if speech[i] { 0 } else { d + 1 };
        out[i] = d;
      }
      out
    };
    let after = distances(&mut (0..speech.len()));
    let before = distances(&mut (0..speech.len()).rev());
    for (name, model, gate) in [
      ("DFN3", DenoiseModel::DeepFilterNet, GATE_AUTO_LOOKAHEAD),
      ("DFN3 no gate", DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0),
      ("DPDFNet", DenoiseModel::DpdfNet, GATE_AUTO_LOOKAHEAD),
      ("RNNoise", DenoiseModel::RnNoise, GATE_AUTO_LOOKAHEAD),
    ] {
      let out = run(model, gate, &mixed);
      let latency = latency_samples(model) + if gate >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
      let aligned = &out[latency..];
      let (mut loud, mut loud6, mut loud15, mut tails, mut tails6) = (0usize, 0usize, 0usize, 0usize, 0usize);
      for (c, o) in clean.as_chunks::<480>().0.iter().zip(aligned.as_chunks::<480>().0).skip(50) {
        let c_db = db(rms(c));
        let loss = c_db - db(rms(o));
        if c_db >= clean_db - 15.0 {
          loud += 1;
          loud6 += (loss > 6.0) as usize;
          loud15 += (loss > 15.0) as usize;
        } else if c_db >= clean_db - 30.0 {
          tails += 1;
          tails6 += (loss > 6.0) as usize;
        }
      }
      let share = |n: usize, of: usize| n as f32 * 100.0 / of.max(1) as f32;
      eprintln!(
        "{name:<12} pauses {:4.1} dB | speech lost > 6 dB {:4.1} %, > 15 dB {:4.1} % | tails lost > 6 dB {:4.1} %",
        measure(&clean, &mixed, &out, latency).noise_suppression_db,
        share(loud6, loud),
        share(loud15, loud),
        share(tails6, tails)
      );
      for (side, own, other) in [("after ", &after, &before), ("before", &before, &after)] {
        let mut line = format!("  {side} speech");
        for (lo, hi) in BUCKETS {
          let (mut e_in, mut e_out, mut n) = (0.0f32, 0.0f32, 0usize);
          for (i, (m, o)) in mixed.as_chunks::<480>().0.iter().zip(aligned.as_chunks::<480>().0).enumerate().skip(50) {
            // A pause frame counts on the side of the nearer word (a tie: after).
            let nearer = if side == "after " { own[i] <= other[i] } else { own[i] < other[i] };
            if (lo..=hi).contains(&own[i]) && nearer {
              e_in += m.iter().map(|x| x * x).sum::<f32>();
              e_out += o.iter().map(|x| x * x).sum::<f32>();
              n += 1;
            }
          }
          line += &format!(" | {lo}-{hi} fr ({n}): {:4.1} dB", 10.0 * (e_in.max(1e-12) / e_out.max(1e-12)).log10());
        }
        eprintln!("{line}");
      }
    }
  }

  /// How far the gate holds a whisper down: the private `whisper_scenario.wav` per model (gate
  /// auto + lookahead), with and without the Silero key (`soft_net`). Per segment the level of
  /// the gated output under the same model's ungated output, dB (0 = the gate stayed open;
  /// `GATE_FLOOR_DB` = closed throughout), and the share of 10 ms frames more than 6 dB under it.
  /// `cargo test -p gamersequalizer-mic-vst --release whisper_gate_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn whisper_gate_report() {
    const TAKE: &str = "../private/whisper_scenario.wav";
    const SEGMENTS: [(&str, f32, f32); 8] = [
      ("silence", 1.0, 18.0),
      ("whisper", 20.0, 24.0),
      ("quieter whisper", 26.5, 31.5),
      ("whisper + typing", 36.0, 41.0),
      ("quiet voice", 45.0, 49.0),
      ("quiet voice 2", 53.5, 57.5),
      ("keys only", 59.5, 68.5),
      ("alternating", 75.0, 82.0),
    ];
    if !std::path::Path::new(FIXTURES).join(TAKE).exists() {
      eprintln!("skipped: put whisper_scenario.wav into test_data/private/");
      return;
    }
    let input = read_wav_48k(TAKE);
    let process = |model: DenoiseModel, gate_db: f32, soft: bool| {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(model);
      isolator.set_gate_threshold_db(gate_db);
      if !soft {
        isolator.soft_voice_prob = f32::INFINITY;
      }
      let mut out = input.clone();
      for block in out.chunks_mut(480) {
        isolator.process_samples(block);
      }
      let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
      out.drain(..latency_samples(model) + lookahead); // aligned with the input
      out
    };
    eprintln!("{:<14}{}", "", SEGMENTS.map(|(name, ..)| format!("{name:>17}")).join(""));
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let ungated = process(model, GATE_OFF_DB - 1.0, false);
      for (label, soft) in [("without", false), ("with Silero", true)] {
        let gated = process(model, GATE_AUTO_LOOKAHEAD, soft);
        let cells = SEGMENTS.map(|(_, from, to)| {
          let range = (from * 48_000.0) as usize..(to * 48_000.0) as usize;
          let part = |x: &'_ [f32]| x.get(range.clone()).unwrap_or_default().to_vec();
          let (g, u) = (part(&gated), part(&ungated));
          let frames = g.as_chunks::<480>().0.iter().zip(u.as_chunks::<480>().0);
          let down = frames.clone().filter(|(g, u)| db(rms(*g)) < db(rms(*u)) - 6.0).count();
          format!("{:>10.1} {:>4.0} %", db(rms(&g)) - db(rms(&u)), down as f32 * 100.0 / frames.count().max(1) as f32)
        });
        eprintln!("{:<14}{}  {label}", format!("{model:?}").chars().take(13).collect::<String>(), cells.join(""));
      }
    }
  }

  #[derive(Debug)]
  struct ClickQuality {
    quality: Quality,
    /// Speech frames with a click: mean dB under the clean speech, shares more than 6 / 15 dB under.
    mean_loss_db: f32,
    lost6: f32,
    lost15: f32,
  }

  /// `speech` (high-passed, -30 dBFS) and ESC-50 mouse clicks hammered: each click (40 ms from
  /// its onset) every 120 ms, peaks at `peak_db` dBFS. Returns (clean, clicks, mixed).
  fn speech_with_clicks(speech: &str, peak_db: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let raw: Vec<f32> = ["1-118206-A-31.wav", "1-18655-A-31.wav"].iter().flat_map(|f| read_wav_48k(f)).collect();
    let max = raw.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-9);
    let mut clicks = Vec::new();
    let mut i = 0;
    while let Some(&s) = raw.get(i) {
      if s.abs() > max * 0.3 {
        let start = i.saturating_sub(48);
        clicks.extend_from_slice(raw.get(start..(start + 48 * 40).min(raw.len())).unwrap_or_default());
        clicks.resize(clicks.len() + 48 * 80, 0.0);
        i = start + 48 * 60;
      } else {
        i += 1;
      }
    }
    let (clean, _) = mix(speech, &KEYBOARD_CLIPS, Some(100.0), -30.0, 0.0);
    while clicks.len() < clean.len() {
      clicks.extend_from_within(..);
    }
    clicks.truncate(clean.len());
    let gain = 10f32.powf(peak_db / 20.0) / max;
    clicks.iter_mut().for_each(|s| *s *= gain);
    let mixed = clean.iter().zip(&clicks).map(|(s, n)| s + n).collect();
    (clean, clicks, mixed)
  }

  fn run_clicks(
    model: DenoiseModel,
    strength: f32,
    gate_db: f32,
    no_rescue: bool,
    clean: &[f32],
    clicks: &[f32],
    mixed: &[f32],
  ) -> ClickQuality {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_strength(strength);
    isolator.set_gate_threshold_db(gate_db);
    isolator.no_rescue = no_rescue;
    let mut out = mixed.to_vec();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
    let latency = latency_samples(model) + lookahead;
    let quality = measure(clean, mixed, &out, latency);
    let clean_db = db(rms(clean));
    let (mut n, mut sum, mut lost6, mut lost15) = (0usize, 0.0f32, 0usize, 0usize);
    for ((c, k), o) in clean
      .as_chunks::<480>()
      .0
      .iter()
      .zip(clicks.as_chunks::<480>().0)
      .zip(out.get(latency..).unwrap_or_default().as_chunks::<480>().0)
      .skip(50)
    {
      let c_db = db(rms(c));
      if c_db >= clean_db - 15.0 && db(rms(k)) > c_db - 10.0 {
        let loss = c_db - db(rms(o));
        n += 1;
        sum += loss.max(0.0);
        lost6 += (loss > 6.0) as usize;
        lost15 += (loss > 15.0) as usize;
      }
    }
    let n = n.max(1) as f32;
    ClickQuality { quality, mean_loss_db: sum / n, lost6: lost6 as f32 / n, lost15: lost15 as f32 / n }
  }

  /// Gate lookahead per model: soft% / loud% lost [pause suppression dB] per mix and the share
  /// of keys-only audio the gate lets through, for 0-30 ms. Private recordings when present,
  /// else the shared fixtures. Picks the `*_LOOKAHEAD_SAMPLES` constants.
  /// `cargo test -p gamersequalizer-mic-vst --release lookahead_sweep -- --ignored --nocapture`
  #[test]
  #[ignore = "report, slow"]
  fn lookahead_sweep() {
    const SPEECH: &str = "../private/speech.wav";
    const KEYS: [&str; 1] = ["../private/keys.wav"];
    let private = [SPEECH, KEYS[0]].iter().all(|f| std::path::Path::new(FIXTURES).join(f).exists());
    let (speech, keys): (&str, &[&str]) = if private { (SPEECH, &KEYS) } else { ("speech_clean.wav", &KEYBOARD_CLIPS) };
    eprintln!(
      "{} recordings; mixes: voice -20/SNR 0, -45/0, -30/-10 dB; keys only -20 dBFS",
      if private { "private" } else { "shared" }
    );
    let mixes: Vec<_> = [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)]
      .iter()
      .map(|&(l, s)| mix(speech, keys, Some(100.0), l, s))
      .collect();
    let keys_only = noise_only(keys, -20.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let ungated = run(model, GATE_OFF_DB - 1.0, &keys_only);
      for ms in [0usize, 5, 10, 15, 20, 30] {
        let la = ms * 48;
        let cells: Vec<String> = mixes
          .iter()
          .map(|(clean, mixed)| {
            let q = measure(clean, mixed, &run_lookahead(model, la, mixed), latency_samples(model) + la);
            format!("{:4.1} / {:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
          })
          .collect();
        let open = open_share(run_lookahead(model, la, &keys_only).get(la..).unwrap_or_default(), &ungated);
        eprintln!("{model:<13?} {ms:>2} ms | {} | keys open {:4.1} %", cells.join(" | "), open * 100.0);
      }
    }
  }

  /// `run` with the gate on and a fixed lookahead instead of the model's.
  fn run_lookahead(model: DenoiseModel, lookahead: usize, input: &[f32]) -> Vec<f32> {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    isolator.lookahead_override = Some(lookahead);
    let mut out = input.to_vec();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    out
  }

  /// Report on one long take of the test script (git-ignored `test_data/private/scenario.wav`).
  /// No clean reference exists, so per second it prints the input level, the raw-input voice
  /// probability and per model the output level without / with the gate (auto + lookahead,
  /// the default) and the gate's open share; then a summary split into seconds with and without
  /// speech, and CPU time. Writes the processed takes to `test_data/private/out/` for listening.
  /// `cargo test -p gamersequalizer-mic-vst --release scenario_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn scenario_report() {
    const SCENARIO: &str = "../private/scenario.wav";
    const SEC: usize = 48_000;
    let models = [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise];
    if !std::path::Path::new(FIXTURES).join(SCENARIO).exists() {
      eprintln!("skipped: put scenario.wav into test_data/private/");
      return;
    }
    let input = read_wav_48k(SCENARIO);
    let secs = input.len() / SEC;
    let out_dir = std::path::Path::new(FIXTURES).join("../private/out");
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut st = DenoiseState::new();
    let mut scratch = [0.0f32; FRAME_SIZE];
    let probs: Vec<f32> = input
      .as_chunks::<FRAME_SIZE>()
      .0
      .iter()
      .map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE)))
      .collect();

    // Per model: (ungated, gated), both aligned with the input.
    let mut outs = Vec::new();
    for model in models {
      let t0 = std::time::Instant::now();
      let ungated = run(model, GATE_OFF_DB - 1.0, &input);
      let cpu = t0.elapsed().as_secs_f32() / (input.len() as f32 / SEC as f32);
      let gated = run(model, GATE_AUTO_LOOKAHEAD, &input);
      let lat = latency_samples(model);
      let ungated = ungated[lat..].to_vec();
      let gated = gated[lat + lookahead_samples(model)..].to_vec();
      eprintln!("{model:?}: CPU {:.1} % of one core (denoiser + input detector, no gate)", cpu * 100.0);
      for (name, sig) in [("nogate", &ungated), ("gate", &gated)] {
        let spec = hound::WavSpec {
          channels: 1,
          sample_rate: 48_000,
          bits_per_sample: 32,
          sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(out_dir.join(format!("{model:?}_{name}.wav")), spec).unwrap();
        sig.iter().for_each(|s| w.write_sample(*s).unwrap());
        w.finalize().unwrap();
      }
      outs.push((ungated, gated));
    }

    let sec_db = |x: &[f32], s: usize| db(rms(x.get(s * SEC..(s + 1) * SEC).unwrap_or_default()));
    // Gate open share in second s: frames where gated is within 6 dB of ungated.
    let open = |g: &[f32], u: &[f32], s: usize| {
      let range = s * SEC..(s + 1) * SEC;
      let (g, u) = (g.get(range.clone()).unwrap_or_default(), u.get(range).unwrap_or_default());
      let f: Vec<bool> = g
        .as_chunks::<480>()
        .0
        .iter()
        .zip(u.as_chunks::<480>().0)
        .filter(|(_, u)| rms(*u) > 1e-6)
        .map(|(g, u)| db(rms(g)) > db(rms(u)) - 6.0)
        .collect();
      f.iter().filter(|&&o| o).count() as f32 / f.len().max(1) as f32 * 100.0
    };
    let voice_share = |s: usize| {
      let p = probs.get(s * 100..(s + 1) * 100).unwrap_or_default();
      p.iter().filter(|&&p| p > VOICE_PROB).count() as f32 / p.len().max(1) as f32 * 100.0
    };
    eprintln!("\n  s   in dB  vad>0.5%  | per model: no gate dB / gate dB / gate open %   (DFN3 | DPDFNet | RNNoise)");
    for s in 0..secs {
      let cols: Vec<String> =
        outs.iter().map(|(u, g)| format!("{:6.1} {:6.1} {:5.1}", sec_db(u, s), sec_db(g, s), open(g, u, s))).collect();
      eprintln!("{s:4} {:6.1} {:7.1}    | {}", sec_db(&input, s), voice_share(s), cols.join(" | "));
    }

    // Seconds with clear speech (raw detector > 0.5 on >= 30 % of frames) vs none (0 %).
    let speech: Vec<usize> = (0..secs).filter(|&s| voice_share(s) >= 30.0).collect();
    let quiet: Vec<usize> = (0..secs).filter(|&s| voice_share(s) == 0.0).collect();
    eprintln!(
      "\nspeech seconds {}, no-speech seconds {}, other {}",
      speech.len(),
      quiet.len(),
      secs - speech.len() - quiet.len()
    );
    let energy = |x: &[f32], set: &[usize]| {
      set
        .iter()
        .map(|&s| x.get(s * SEC..(s + 1) * SEC).unwrap_or_default().iter().map(|v| v * v).sum::<f32>())
        .sum::<f32>()
        .max(1e-12)
    };
    for (model, (u, g)) in models.iter().zip(&outs) {
      // Voice frames (raw detector > 0.5) where the gate was closed (gated > 6 dB under ungated).
      let (mut vf, mut cut) = (0usize, 0usize);
      for (i, p) in probs.iter().enumerate() {
        let r = i * FRAME_SIZE..(i + 1) * FRAME_SIZE;
        if let (true, Some(gf), Some(uf)) = (*p > VOICE_PROB, g.get(r.clone()), u.get(r)) {
          vf += 1;
          cut += (rms(uf) > 1e-6 && db(rms(gf)) <= db(rms(uf)) - 6.0) as usize;
        }
      }
      eprintln!(
        "{model:<13?} no-speech seconds suppressed {:5.1} dB (no gate {:5.1}) | speech seconds level {:+5.1} dB vs input (no gate {:+5.1}) | voice frames under closed gate {:.2} %",
        10.0 * (energy(&input, &quiet) / energy(g, &quiet)).log10(),
        10.0 * (energy(&input, &quiet) / energy(u, &quiet)).log10(),
        10.0 * (energy(g, &speech) / energy(&input, &speech)).log10(),
        10.0 * (energy(u, &speech) / energy(&input, &speech)).log10(),
        cut as f32 / vf.max(1) as f32 * 100.0
      );
    }
  }

  /// Whisper analysis, step 1 (`docs/whisper-step1-results.md`): what each possible gate key sees on
  /// whisper vs. voice, room noise and typing, on a recording of the whisper script. Prints
  /// distributions, no thresholds. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release whisper_probe -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn whisper_probe() {
    // Recorded from the whisper script; bounds found with `vad_timeline` (pauses between sections).
    const SCENARIO: &str = "../private/whisper_scenario.wav";
    const KEYS: &str = "../private/keys.wav";
    const HALF_SEC: usize = 24_000;
    if [SCENARIO, KEYS].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
      eprintln!("skipped: put whisper_scenario.wav and keys.wav into test_data/private/");
      return;
    }
    let signals = [read_wav_48k(SCENARIO), read_wav_48k(KEYS)];
    let keys_len = signals[1].len();
    let s = |a: usize, b: usize| a * HALF_SEC..b * HALF_SEC; // in half seconds
                                                             // (name, signal index, samples) -- 10 ms frame ranges are derived from these.
    let segments: [(&str, usize, std::ops::Range<usize>); 9] = [
      ("room 1-18 s", 0, s(2, 36)),
      ("whisper 20-24 s", 0, s(40, 48)),
      ("quiet whisp 26.5-31.5", 0, s(53, 63)),
      ("whisp+type 36-41 s", 0, s(72, 82)),
      ("voice 45-49 s", 0, s(90, 98)),
      ("voice 53.5-57.5 s", 0, s(107, 115)),
      ("keys only 59.5-68.5", 0, s(119, 137)),
      ("alternate 75-82 s", 0, s(150, 164)),
      ("keys.wav", 1, 0..keys_len),
    ];
    let frames = |r: &std::ops::Range<usize>| r.start / FRAME_SIZE..r.end / FRAME_SIZE;
    let probs = |x: &[f32], gain_db: f32| -> Vec<f32> {
      let g = 10f32.powf(gain_db / 20.0) * I16_SCALE;
      let mut st = DenoiseState::new();
      let mut scratch = [0.0f32; FRAME_SIZE];
      x.as_chunks::<FRAME_SIZE>()
        .0
        .iter()
        .map(|f| st.process_frame(&mut scratch, &f.map(|s| (s * g).clamp(-32768.0, 32767.0))))
        .collect()
    };
    let frame_db = |x: &[f32]| -> Vec<f32> { x.as_chunks::<FRAME_SIZE>().0.iter().map(|f| db(rms(f))).collect() };
    let pct = |v: &[f32], p: f32| -> f32 {
      let mut s = v.to_vec();
      s.sort_by(f32::total_cmp);
      s.get(((s.len().max(1) - 1) as f32 * p) as usize).copied().unwrap_or(f32::NAN)
    };
    let share = |v: &[bool]| v.iter().filter(|&&b| b).count() as f32 / v.len().max(1) as f32 * 100.0;
    let prob_stats = |v: &[f32]| {
      let over = |t: f32| v.iter().filter(|&&p| p > t).count() as f32 / v.len().max(1) as f32 * 100.0;
      format!(
        "p50 {:.2} p90 {:.2} max {:.2} | >0.05 {:4.1}% >0.3 {:4.1}% >0.5 {:4.1}%",
        pct(v, 0.5),
        pct(v, 0.9),
        pct(v, 1.0),
        over(0.05),
        over(0.3),
        over(0.5)
      )
    };
    // Clamped: model outputs are shorter than the input by their latency.
    let slice = |v: &[f32], r: std::ops::Range<usize>| {
      v.get(r.start.min(v.len())..r.end.min(v.len())).unwrap_or_default().to_vec()
    };

    eprintln!("== RNNoise voice probability on the raw input, gain applied only to the detector");
    for gain in [0.0f32, 12.0, 24.0] {
      let p: Vec<Vec<f32>> = signals.iter().map(|s| probs(s, gain)).collect();
      for (name, i, r) in &segments {
        eprintln!("  +{gain:2} dB {name:<20} {}", prob_stats(&slice(&p[*i], frames(r))));
      }
    }

    // Ungated model outputs, aligned with the input.
    let models = [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise];
    let outs: Vec<Vec<Vec<f32>>> = models
      .iter()
      .map(|&m| {
        signals
          .iter()
          .map(|s| run(m, GATE_OFF_DB - 1.0, s).get(latency_samples(m)..).unwrap_or_default().to_vec())
          .collect()
      })
      .collect();
    eprintln!(
      "\n== Output detector (RNNoise probability on the ungated model output) and output level per 10 ms frame"
    );
    for (m, out) in models.iter().zip(&outs) {
      let p: Vec<Vec<f32>> = out.iter().map(|s| probs(s, 0.0)).collect();
      let l: Vec<Vec<f32>> = out.iter().map(|s| frame_db(s)).collect();
      for (name, i, r) in &segments {
        let lv = slice(&l[*i], frames(r));
        eprintln!(
          "  {m:<13?} {name:<20} {} | dBFS p10 {:6.1} p50 {:6.1} p90 {:6.1} max {:6.1}",
          prob_stats(&slice(&p[*i], frames(r))),
          pct(&lv, 0.1),
          pct(&lv, 0.5),
          pct(&lv, 0.9),
          pct(&lv, 1.0)
        );
      }
    }

    // DFN3 local SNR per hop (the value `DeepFilterEngine::process` drops), default strength.
    let lsnr: Vec<Vec<f32>> = signals
      .iter()
      .map(|s| {
        let mut e = DeepFilterEngine::load().unwrap();
        e.model.atten_lim = atten_lim(0.75);
        assert_eq!(e.hop, FRAME_SIZE);
        s.as_chunks::<FRAME_SIZE>()
          .0
          .iter()
          .map(|f| {
            e.in_frame.iter_mut().zip(f).for_each(|(d, x)| *d = *x);
            e.model.process(e.in_frame.view(), e.out_frame.view_mut()).unwrap()
          })
          .collect()
      })
      .collect();
    eprintln!(
      "\n== DFN3 lsnr (dB); min over the whole file: {:.1}",
      lsnr.iter().flatten().copied().fold(f32::MAX, f32::min)
    );
    let runs = |v: &[bool], n: u32| -> Vec<bool> {
      let mut c = 0u32;
      v.iter()
        .map(|&b| {
          c = if b { c + 1 } else { 0 };
          c >= n
        })
        .collect()
    };
    for (name, i, r) in &segments {
      let v = slice(&lsnr[*i], frames(r));
      let key = |t: f32, n: u32| share(&runs(&v.iter().map(|&x| x > t).collect::<Vec<_>>(), n));
      eprintln!(
        "  {name:<20} p10 {:5.1} p50 {:5.1} p90 {:5.1} max {:5.1} | key lsnr>0 x5 {:4.1}% >5 x5 {:4.1}% >10 x5 {:4.1}% >10 x10 {:4.1}%",
        pct(&v, 0.1),
        pct(&v, 0.5),
        pct(&v, 0.9),
        pct(&v, 1.0),
        key(0.0, 5),
        key(5.0, 5),
        key(10.0, 5),
        key(10.0, 10)
      );
    }

    // Idea N2: DFN3 output energy over a tracked floor (rises 2 dB/s, falls at once, >= -75 dBFS).
    eprintln!(
      "\n== N2 key: share of frames the DFN3 output energy key would open (open dB over floor x frames in a row)"
    );
    let n2 = |x: &[f32], open_db: f32, min_frames: u32| -> Vec<bool> {
      let mut floor = -75.0f32;
      let above: Vec<bool> = frame_db(x)
        .iter()
        .map(|&l| {
          floor = if l < floor { l } else { floor + 0.02 }.max(-75.0);
          l > floor + open_db
        })
        .collect();
      runs(&above, min_frames)
    };
    // With the gate's hold (200 ms after the last keyed frame), what the gate would actually do.
    let held = |k: &[bool]| -> Vec<bool> {
      let hold = (GATE_HOLD_SAMPLES / FRAME_SIZE) as u32;
      let mut left = 0u32;
      k.iter()
        .map(|&b| {
          left = if b { hold } else { left.saturating_sub(1) };
          left > 0
        })
        .collect()
    };
    let configs = [(12.0f32, 5u32), (9.0, 5), (15.0, 5), (12.0, 10)];
    let keyed: Vec<Vec<Vec<bool>>> =
      configs.iter().map(|&(o, n)| outs[0].iter().map(|s| n2(s, o, n)).collect()).collect();
    for (label, with_hold) in [("raw", false), ("+200 ms hold", true)] {
      eprintln!("  -- {label}");
      for (name, i, r) in &segments {
        let cols: Vec<String> = configs
          .iter()
          .zip(&keyed)
          .map(|((o, n), k)| {
            let k = if with_hold { held(&k[*i]) } else { k[*i].clone() };
            let f = frames(r);
            format!(
              "{o:2}dB x{n:2}: {:5.1}%",
              share(k.get(f.start.min(k.len())..f.end.min(k.len())).unwrap_or_default())
            )
          })
          .collect();
        eprintln!("  {name:<20} {}", cols.join(" | "));
      }
    }
  }

  /// Whisper analysis, round 2 (`docs/whisper-step1-results.md`, chapter 7): sweep of the DFN3
  /// output energy gate key (floor source x threshold x run counter x hold), scored in energy, plus
  /// whisper under `keys.wav` and DFN3 on room noise amplified. The gate is simulated per 10 ms frame
  /// (open = 0 dB, closed = the floor), without the RNNoise detectors (they never open on whisper).
  /// `cargo test -p gamersequalizer-mic-vst --release whisper_sweep -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn whisper_sweep() {
    const SCENARIO: &str = "../private/whisper_scenario.wav";
    const KEYS: &str = "../private/keys.wav";
    const HALF_SEC: usize = 24_000;
    if [SCENARIO, KEYS].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
      eprintln!("skipped: put whisper_scenario.wav and keys.wav into test_data/private/");
      return;
    }
    let lat = latency_samples(DenoiseModel::DeepFilterNet);
    let dfn = |x: &[f32]| -> Vec<f32> {
      run(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, x).get(lat..).unwrap_or_default().to_vec()
    };
    let inputs = [read_wav_48k(SCENARIO), read_wav_48k(KEYS)];
    let outs: Vec<Vec<f32>> = inputs.iter().map(|x| dfn(x)).collect();
    let s = |a: usize, b: usize| a * HALF_SEC / FRAME_SIZE..b * HALF_SEC / FRAME_SIZE; // frames
    let keys_frames = outs[1].len() / FRAME_SIZE;
    let segments = [
      ("whisper", 0, s(40, 48)),
      ("quiet wh", 0, s(53, 63)),
      ("wh+type", 0, s(72, 82)),
      ("voice", 0, s(90, 98)),
      ("room", 0, s(2, 36)),
      ("keys", 0, s(119, 137)),
      ("keys.wav", 1, 0..keys_frames),
    ];
    let frame_db = |x: &[f32]| -> Vec<f32> { x.as_chunks::<FRAME_SIZE>().0.iter().map(|f| db(rms(f))).collect() };
    let frame_e =
      |x: &[f32]| -> Vec<f32> { x.as_chunks::<FRAME_SIZE>().0.iter().map(|f| f.iter().map(|v| v * v).sum()).collect() };
    let pct = |v: &[f32], p: f32| -> f32 {
      let mut s = v.to_vec();
      s.sort_by(f32::total_cmp);
      s.get(((s.len().max(1) - 1) as f32 * p) as usize).copied().unwrap_or(f32::NAN)
    };

    // 1c: frame-level spread of the raw input and the DFN3 output in the quiet room.
    let room = s(2, 36);
    for (name, x) in [("raw input", &inputs[0]), ("DFN3 output", &outs[0])] {
      let l = frame_db(x);
      let l = l.get(room.clone()).unwrap_or_default();
      eprintln!(
        "room {name:<12} per 10 ms: p10 {:6.1} p50 {:6.1} p90 {:6.1} p99 {:6.1} dBFS",
        pct(l, 0.1),
        pct(l, 0.5),
        pct(l, 0.9),
        pct(l, 0.99)
      );
    }

    // The key: floor from raw input or from the output, level from the output.
    let key = |inp: &[f32], out: &[f32], from_input: bool, open_db: f32, forgive: bool, hold: u32| -> Vec<bool> {
      let (li, lo) = (frame_db(inp), frame_db(out));
      let mut floor = -75.0f32;
      let (mut run, mut below, mut left) = (0u32, 0u32, 0u32);
      lo.iter()
        .zip(&li)
        .map(|(&o, &i)| {
          let src = if from_input { i } else { o };
          floor = if src < floor { src } else { floor + 0.02 }.max(-75.0);
          if o > floor + open_db {
            run += 1;
            below = 0;
          } else {
            below += 1;
            if !forgive || below >= 2 {
              run = 0;
            }
          }
          left = if run >= 5 { hold.max(1) } else { left.saturating_sub(1) };
          left > 0
        })
        .collect()
    };
    // Energy with the simulated gate over energy without it, dB (0 = gate always open).
    let closed = 10f32.powf(2.0 * GATE_FLOOR_DB / 20.0);
    let loss = |e: &[f32], k: &[bool], r: std::ops::Range<usize>| -> f32 {
      let (mut g, mut u) = (1e-12f32, 1e-12f32);
      for (e, k) in e.get(r.clone()).unwrap_or_default().iter().zip(k.get(r).unwrap_or_default()) {
        u += e;
        g += if *k { *e } else { e * closed };
      }
      10.0 * (g / u).log10()
    };
    let energies: Vec<Vec<f32>> = outs.iter().map(|o| frame_e(o)).collect();
    eprintln!("\n== Sweep. Speech columns: loss vs no gate (0 best). Noise columns: noise let through (0 = gate shut, 12 = open)");
    eprintln!(
      "  floor  open forgive hold | {}",
      segments.iter().map(|(n, _, _)| format!("{n:>8}")).collect::<Vec<_>>().join(" ")
    );
    for from_input in [false, true] {
      for open_db in [6.0f32, 9.0, 12.0] {
        for forgive in [false, true] {
          for hold_ms in [0u32, 50, 100, 200] {
            let keyed: Vec<Vec<bool>> =
              inputs.iter().zip(&outs).map(|(i, o)| key(i, o, from_input, open_db, forgive, hold_ms / 10)).collect();
            let cols: Vec<String> = segments
              .iter()
              .enumerate()
              .map(|(n, (_, i, r))| {
                let l = loss(&energies[*i], &keyed[*i], r.clone());
                format!("{:8.1}", if n < 4 { l } else { l - GATE_FLOOR_DB })
              })
              .collect();
            eprintln!(
              "  {:<6} {open_db:4} {:<7} {hold_ms:4} | {}",
              if from_input { "input" } else { "output" },
              forgive,
              cols.join(" ")
            );
          }
        }
      }
    }

    // Step 2: clean whisper (20-24 s + 26.5-31.5 s) under keys.wav, DFN3 without gate.
    let sc = &inputs[0];
    let whisper: Vec<f32> = [40..48, 53..63]
      .iter()
      .flat_map(|r| sc.get(r.start * HALF_SEC..r.end * HALF_SEC).unwrap_or_default().iter().copied())
      .collect();
    let keys: Vec<f32> = inputs[1].iter().cycle().take(whisper.len()).copied().collect();
    eprintln!("\n== Whisper under keys.wav, DFN3 without gate (dropouts: frames > 15 dB under the clean whisper)");
    for snr in [None, Some(0.0f32), Some(-10.0)] {
      let mixed: Vec<f32> = match snr {
        None => whisper.clone(),
        Some(snr) => {
          let g = rms(&whisper) / rms(&keys).max(1e-9) / 10f32.powf(snr / 20.0);
          whisper.iter().zip(&keys).map(|(w, k)| w + k * g).collect()
        }
      };
      let out = run(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &mixed);
      let q = measure(&whisper, &mixed, &out, lat);
      eprintln!(
        "  SNR {snr:?}: loud whisper lost {:4.1} %, soft {:4.1} %",
        q.dropouts * 100.0,
        q.soft_dropouts * 100.0
      );
    }

    // What each model alone (no gate) keeps of the whisper sections, energy out / in, both after
    // the default EQ's 100 Hz low cut (breath pops and desk thumps are removed there anyway).
    let low_cut = |x: &[f32]| {
      let mut eq = BiquadCascade::default();
      let band = BiquadBand {
        filter_type: FilterType::HighPass,
        frequency: 100.0,
        q: 0.707,
        slope_db: 48,
        ..BiquadBand::default()
      };
      eq.set_bands(&[band], 48_000.0);
      let mut y = x.to_vec();
      eq.process(&mut y);
      y
    };
    let sc_raw = sc;
    let sc_lc = low_cut(sc_raw);
    eprintln!("\n== Model alone on the recorded sections, output energy vs input (dB), after a 100 Hz low cut");
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let sc = &sc_lc;
      let l = latency_samples(model);
      let out = low_cut(&run(model, GATE_OFF_DB - 1.0, sc_raw));
      let ratio = |a: usize, b: usize| {
        let e = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>().max(1e-12);
        let o = out.get(a * HALF_SEC + l..b * HALF_SEC + l).unwrap_or_default();
        10.0 * (e(o) / e(sc.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default())).log10()
      };
      eprintln!(
        "  {model:<13?} whisper {:6.1} | quiet whisper {:6.1} | whisper+type {:6.1} | voice {:6.1} | room {:6.1}",
        ratio(40, 48),
        ratio(53, 63),
        ratio(72, 82),
        ratio(90, 98),
        ratio(2, 36)
      );
    }

    // A "whisper mode" candidate: lower strength, no gate. Whisper kept vs noise removed.
    eprintln!(
      "\n== Strength sweep, no gate: whisper / quiet whisper / whisper+type kept, room / keys.wav removed (dB)"
    );
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
      for strength in [0.25f32, 0.4, 0.5, 0.75] {
        let l = latency_samples(model);
        let go = |x: &[f32]| {
          let mut iso = AiVoiceIsolator::new();
          iso.set_model(model);
          iso.set_gate_threshold_db(GATE_OFF_DB - 1.0);
          iso.set_strength(strength);
          let mut o = x.to_vec();
          o.chunks_mut(480).for_each(|b| iso.process_samples(b));
          o
        };
        let e = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>().max(1e-12);
        let (o, ok) = (go(sc), go(&inputs[1]));
        let ratio = |a: usize, b: usize| {
          10.0
            * (e(o.get(a * HALF_SEC + l..b * HALF_SEC + l).unwrap_or_default())
              / e(sc.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default()))
            .log10()
        };
        let keys_removed = 10.0 * (e(&inputs[1]) / e(ok.get(l..).unwrap_or_default())).log10();
        eprintln!(
          "  {model:<13?} {strength:4}: whisper {:5.1} {:5.1} {:5.1} | room {:5.1} keys.wav {:5.1}",
          ratio(40, 48),
          ratio(53, 63),
          ratio(72, 82),
          -ratio(2, 36),
          keys_removed
        );
      }
    }

    // Chapter 7: does DFN3 suppress quiet room noise better when fed 20 dB hotter?
    let room_in = sc.get(HALF_SEC * 2..HALF_SEC * 36).unwrap_or_default();
    for gain_db in [0.0f32, 20.0] {
      let g = 10f32.powf(gain_db / 20.0);
      let hot: Vec<f32> = room_in.iter().map(|x| x * g).collect();
      let out: Vec<f32> = dfn(&hot).iter().map(|x| x / g).collect();
      let skip = 100 * FRAME_SIZE; // warm-up
      let e = |x: &[f32]| x.get(skip..).unwrap_or_default().iter().map(|v| v * v).sum::<f32>().max(1e-12);
      eprintln!("room noise into DFN3 at +{gain_db:2} dB: suppressed {:5.1} dB", 10.0 * (e(room_in) / e(&out)).log10());
    }
  }

  /// DeepFilterNet 3 alone (no gate) on speech over keys, strength 0.75 and 1.0: time per second
  /// of audio, and the output against a reference saved earlier (raw f32 LE, `DFN3_REF`; written
  /// when missing). For runtime upgrades (tract 0.21 -> 0.23, 2026-11-08). Run:
  /// `$env:DFN3_REF='<file>'; cargo test -p gamersequalizer-mic-vst --release dfn3_reference -- --ignored --nocapture`
  #[test]
  #[ignore = "runtime upgrade check"]
  fn dfn3_reference() {
    let Ok(path) = std::env::var("DFN3_REF") else { return eprintln!("skipped: set DFN3_REF") };
    let (_, mixed) = speech_over_keyboard(-30.0, 0.0);
    let mut out = Vec::with_capacity(2 * mixed.len());
    let mut secs = Vec::new();
    for strength in [0.75f32, 1.0] {
      let t = Instant::now();
      let mut df = DeepFilterEngine::load().unwrap();
      eprintln!("DFN3 load: {} ms", t.elapsed().as_millis());
      let mut x = mixed.clone();
      let t = Instant::now();
      for block in x.chunks_mut(480) {
        df.process(block, strength, 1.0);
      }
      secs.push(t.elapsed().as_secs_f32() / (mixed.len() as f32 / 48_000.0) * 100.0);
      out.extend_from_slice(&x);
    }
    eprintln!("DFN3 cpu: {:.2} % (0.75), {:.2} % (1.0) of a core", secs[0], secs[1]);
    match std::fs::read(&path) {
      Ok(bytes) => {
        let reference: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(reference.len(), out.len());
        let max_diff = reference.iter().zip(&out).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        let err: Vec<f32> = reference.iter().zip(&out).map(|(a, b)| a - b).collect();
        eprintln!("vs reference: max |diff| {max_diff:.2e}, signal/diff {:.1} dB", db(rms(&reference)) - db(rms(&err)));
      }
      Err(_) => {
        let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&path, bytes).unwrap();
        eprintln!("reference written to {path}");
      }
    }
  }

  /// Does echo AEC3 leaves behind still open the gate (the case an `echo_only` mask would cover)?
  /// The debug captures' raw mic after AEC3 (`aec3::tests::capture_export` writes them to
  /// `test_data/private/aec_ab/`) through each model: share of 10 ms blocks the input detector
  /// hears speech / the gate is open in, in "echo only" blocks (far end playing, AEC3 took at least
  /// 15 dB off the mic: little near-end voice in them) and in blocks without a far end. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release echo_gate_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn echo_gate_report() {
    let base = std::path::Path::new(FIXTURES).join("../private");
    let Ok(dirs) = std::fs::read_dir(base.join("captures")) else { return eprintln!("no captures") };
    eprintln!(
      "capture          model          echo-only blocks: voice% gate% out dBFS | no far end: blocks voice% gate%"
    );
    for dir in dirs.flatten().map(|d| d.path()) {
      let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
      let aec3_file = format!("../private/aec_ab/{name}_aec3.wav");
      if !std::path::Path::new(FIXTURES).join(&aec3_file).exists() {
        continue;
      }
      let mic = read_wav_48k(&format!("../private/captures/{name}/01_mic_raw.wav"));
      let refs = read_wav_48k(&format!("../private/captures/{name}/03_ref_aec.wav"));
      let aec3 = read_wav_48k(&aec3_file);
      let blocks = mic.len().min(refs.len()).min(aec3.len()) / 480;
      // 0 = no far end, 1 = echo only, 2 = far end + near-end voice (not counted)
      let kind: Vec<u8> = (0..blocks)
        .map(|b| {
          let r = b * 480..(b + 1) * 480;
          let (m, f, a) = (rms(&mic[r.clone()]), rms(&refs[r.clone()]), rms(&aec3[r]));
          if db(f) < -50.0 {
            0
          } else if db(a) <= db(m) - 15.0 {
            1
          } else {
            2
          }
        })
        .collect();
      for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet] {
        let mut isolator = AiVoiceIsolator::new();
        isolator.set_model(model);
        isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
        let mut out = aec3.clone();
        let (mut voice, mut open) = (Vec::with_capacity(blocks), Vec::with_capacity(blocks));
        for block in out.chunks_mut(480).take(blocks) {
          isolator.process_samples(block);
          voice.push(isolator.heard_voice());
          open.push(isolator.gate.gain > 0.5);
        }
        // The gate acts on audio a model latency + lookahead older than what was just heard.
        let lead = isolator.output_lead_samples().div_ceil(480);
        let stats = |k: u8| {
          let idx: Vec<usize> = (100..blocks.saturating_sub(lead)).filter(|&b| kind[b] == k).collect();
          let share = |v: &[bool], off: usize| {
            idx.iter().filter(|&&b| v.get(b + off).copied().unwrap_or(false)).count() as f32 * 100.0
              / idx.len().max(1) as f32
          };
          let power = idx
            .iter()
            .map(|&b| out.get((b + lead) * 480..(b + lead + 1) * 480).map_or(0.0, |s| rms(s).powi(2)))
            .sum::<f32>()
            / idx.len().max(1) as f32;
          (idx.len(), share(&voice, 0), share(&open, lead), db(power.sqrt()))
        };
        let (e, n) = (stats(1), stats(0));
        eprintln!(
          "{name} {model:<13?} {:6} {:6.1} {:6.1} {:7.1} | {:6} {:6.1} {:6.1}",
          e.0, e.1, e.2, e.3, n.0, n.1, n.2
        );
      }
    }
  }

  /// The learning voice leveler on the private recordings: denoiser (RNNoise, default gate) +
  /// leveler, the input at its own level and 12 dB / 24 dB lower (a mic set low). Prints the gain
  /// over time and the output level of voice and room sections. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release leveler_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn leveler_report() {
    use crate::voice_leveler::VoiceLeveler;
    const HALF_SEC: usize = 24_000;
    let path = "../private/whisper_scenario.wav";
    if !std::path::Path::new(FIXTURES).join(path).exists() {
      return eprintln!("skipped: put whisper_scenario.wav into test_data/private/");
    }
    let input = read_wav_48k(path);
    let e_db = |x: &[f32], a: usize, b: usize| {
      let s = x.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default();
      10.0 * (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).max(1e-12).log10()
    };
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet] {
      for att_db in [0.0f32, -12.0, -24.0] {
        let g = 10f32.powf(att_db / 20.0);
        let mut x: Vec<f32> = input.iter().map(|s| s * g).collect();
        let mut iso = AiVoiceIsolator::new();
        iso.set_model(model);
        iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
        let mut lv = VoiceLeveler::new();
        let mut gains = Vec::new();
        for (i, b) in x.chunks_mut(480).enumerate() {
          iso.process_samples(b);
          lv.process(b, iso.heard_voice(), iso.output_lead_samples());
          if i % 50 == 49 {
            gains.push(lv.gain_db());
          }
        }
        let lead = latency_samples(model) + lookahead_samples(model);
        let out = x.get(lead..).unwrap_or_default();
        let at = |s: f32| gains.get((s * 2.0) as usize).copied().unwrap_or(f32::NAN);
        eprintln!(
          "{model:<13?} input {att_db:+3} dB | gain at 20 s {:+5.1}, 46 s {:+5.1}, 47 s {:+5.1}, 50 s {:+5.1}, 60 s {:+5.1}, 84 s {:+5.1} | out: voice 45-49 {:6.1} voice 53-57 {:6.1} alternate {:6.1} room {:6.1} whisper {:6.1} dBFS",
          at(20.0),
          at(46.0),
          at(47.0),
          at(50.0),
          at(60.0),
          at(83.5),
          e_db(out, 90, 98),
          e_db(out, 107, 115),
          e_db(out, 150, 164),
          e_db(out, 2, 36),
          e_db(out, 40, 48)
        );
      }
    }
  }

  /// Per 0.5 s of a private recording: level, raw RNNoise voice share (> 0.5) and clicky 10 ms
  /// frames (peak 20 dB over RMS: keys). For finding section bounds. Run (file in test_data/private/):
  /// `$env:WAV="x.wav"; cargo test -p gamersequalizer-mic-vst --release vad_timeline -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn vad_timeline() {
    let Ok(name) = std::env::var("WAV") else { return eprintln!("skipped: set WAV") };
    let x = read_wav_48k(&format!("../private/{name}"));
    let mut st = DenoiseState::new();
    let mut scratch = [0.0f32; FRAME_SIZE];
    let frames = x.as_chunks::<FRAME_SIZE>().0;
    let p: Vec<f32> = frames.iter().map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE))).collect();
    for (i, (pc, fc)) in p.chunks(50).zip(frames.chunks(50)).enumerate() {
      let all: Vec<f32> = fc.iter().flatten().copied().collect();
      let voice = pc.iter().filter(|&&v| v > VOICE_PROB).count() * 2;
      let clicks = fc.iter().filter(|f| db(f.iter().fold(0.0f32, |m, s| m.max(s.abs()))) - db(rms(*f)) > 20.0).count();
      eprintln!(
        "{:5.1} {:6.1} dB voice {voice:3}% clicks {clicks:2} {}",
        i as f32 * 0.5,
        db(rms(&all)),
        "#".repeat(voice / 5)
      );
    }
  }

  /// Tuning report for the detector's hysteresis (keep threshold x active window). Run:
  /// `cargo test -p gamersequalizer-mic-vst --release gate_sweep -- --ignored --nocapture`
  #[test]
  #[ignore = "tuning report, slow"]
  fn gate_sweep() {
    let run_with = |model: DenoiseModel, keep: f32, active: u32, input: &[f32]| -> Vec<f32> {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(model);
      isolator.set_gate_threshold_db(GATE_AUTO);
      isolator.vad.keep_prob = keep;
      isolator.vad.active_frames = active;
      let mut out = input.to_vec();
      for block in out.chunks_mut(480) {
        isolator.process_samples(block);
      }
      out
    };
    let speech = [(-30.0f32, -10.0f32), (-20.0, 0.0), (-45.0, 0.0)].map(|(l, s)| speech_over_keyboard(l, s));
    // Speech, then typing only: how long after the last word the gate lets keys through.
    let (clean, _) = speech_over_keyboard(-30.0, 0.0);
    let last_loud = clean.iter().rposition(|s| s.abs() > 0.01).unwrap_or(clean.len());
    let typing = keyboard_only(-20.0);
    let mut talk_then_type: Vec<f32> = clean[..last_loud].to_vec();
    let speech_end = talk_then_type.len();
    talk_then_type.extend_from_slice(&typing);
    // How often non-speech scores above a keep threshold (it would hold the gate open).
    let mut x = 1u32;
    let mut lp = 0.0f32;
    let fan: Vec<f32> = (0..48_000 * 10)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        lp += 0.05 * (((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) - lp);
        lp
      })
      .collect();
    for (name, sig) in [
      ("keys -20", keyboard_only(-20.0)),
      ("keys -40", keyboard_only(-40.0)),
      ("fan -50", scale_to_db(&fan, -50.0)),
      ("fan -30", scale_to_db(&fan, -30.0)),
    ] {
      let mut st = DenoiseState::new();
      let mut scratch = [0.0f32; FRAME_SIZE];
      let probs: Vec<f32> = sig
        .as_chunks::<FRAME_SIZE>()
        .0
        .iter()
        .skip(50)
        .map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE)))
        .collect();
      let share = |t: f32| probs.iter().filter(|&&p| p > t).count() as f32 / probs.len() as f32 * 100.0;
      eprintln!(
        "{name}: frames over 0.05 {:.1} %, 0.1 {:.1} %, 0.2 {:.1} %, 0.5 {:.1} %",
        share(0.05),
        share(0.1),
        share(0.2),
        share(0.5)
      );
    }
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
      let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
      eprintln!(
        "
{model:?}  soft%/loud% lost [pause suppression dB]: keys-10dB | -20dBFS | -45dBFS ; open% while typing after speech"
      );
      for (active, keep) in
        [(20u32, 0.5f32), (20, 0.1), (20, 0.05), (10, 0.1), (10, 0.05), (5, 0.1), (5, 0.05), (5, 0.2)]
      {
        let q: Vec<String> = speech
          .iter()
          .map(|(c, m)| {
            let q = measure(c, m, &run_with(model, keep, active, m), latency);
            format!("{:4.1}/{:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
          })
          .collect();
        let out = run_with(model, keep, active, &talk_then_type);
        let ungated = run(model, GATE_OFF_DB - 1.0, &talk_then_type);
        let tail = open_share(&out[speech_end - 48_000..], &ungated[speech_end - 48_000..]); // skips its first 1 s
        eprintln!("  active {active:2} keep {keep:4}: {} ; {:4.1}", q.join(" | "), tail * 100.0);
      }
      for (c, m) in &speech {
        let q = measure(c, m, &run(model, GATE_OFF_DB - 1.0, m), latency);
        eprint!(
          "  no gate: {:4.1}/{:3.1} [{:4.1}]",
          q.soft_dropouts * 100.0,
          q.dropouts * 100.0,
          q.noise_suppression_db
        );
      }
      eprintln!();
    }
  }
}
