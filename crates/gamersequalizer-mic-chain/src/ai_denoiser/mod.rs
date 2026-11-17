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

mod deepfilter;
mod gate;
mod loader;
mod rescue;
mod rnnoise;
mod sleep;
mod voice;

use deepfilter::*;
use gate::*;
use loader::*;
use rescue::*;
use rnnoise::*;
use sleep::*;
use voice::*;

/// Attenuation limit of the heavy models at strength 0+ .. just below 1.0 (100 % = unlimited).
const DF_MIN_ATTEN_DB: f32 = 6.0;
const DF_MAX_ATTEN_DB: f32 = 60.0;

/// Silero VAD on the raw input (`soft_net`) opens the gate above this, next to the detector
/// above. It never reaches it on keys or mouse clicks alone (highest 0.08 on the ESC-50 keys)
/// and hears 70-95 % of whispered frames, where `in_net` at its keep threshold hears 30-62 %
/// (DEVLOG milestone 38). In the chain (`private_report`, gate auto + lookahead, private voice
/// under private keys): soft syllables the gate cut 4.0 -> 2.4 % (DFN3, voice -45 dBFS; 2.4 %
/// is the model alone) and 2.1-2.4 -> 0.3 % (RNNoise), for 0.1-2.2 dB less key suppression in the
/// short pauses between words; the gate on keys alone opens exactly as often as before. At 0.5:
/// 3.7 % and 1.2-2.3 %. `whisper_gate_report`: whispered frames under a closed gate 40 -> 2 %.
const SOFT_VOICE_PROB: f32 = 0.15;

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
/// No voice on the raw-input detectors (`in_net`, Silero) for this long stops the click rescue
/// and its two RNNoise passes; a voice starts them again, and the rescue waits
/// `STAND_IN_WARMUP_SAMPLES` for RNNoise to warm up (the first 300 ms after a long pause go
/// without it). Typing alone at -30 dBFS: RNNoise ran in 14 % of the blocks, DFN3 awake
/// 3.8 -> 3.4 % of a core. Quality reports unchanged (`click_report`: frames under hammered
/// clicks lost > 6 dB 12.6 -> 13.3 %, the rest equal; 2026-11-16).
const RESCUE_IDLE_SAMPLES: usize = 48_000 * 2;
/// The leveler learns the speech level only from frames `in_net` rates above this for
/// `LEVEL_MIN_FRAMES` in a row (60 ms). `in_net` at the gate's thresholds (0.7 / 0.2) hears
/// 29 % of a whisper's frames; the leveler took it for the speech level, learned it for seconds
/// before the voice and then followed it up only 0.65 dB per second of voice: DFN3 +13.6 dB, DPDFNet
/// +18.5 dB at 84 s against +7.1 for RNNoise, the voice 6-14 dB over the target (`leveler_report`).
/// `leveler_key_report` on the private scenario: whisper 1.2 % (the 2017 network 2.2 %), voice 73 %
/// (2017: 77 %), at no cost: the probability is there already.
const LEVEL_PROB: f32 = 0.95;
const LEVEL_MIN_FRAMES: u32 = 6;
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

/// Added latency, 48 kHz samples. Both = output FIFO priming (one frame, so any block size
/// works) + the model's own delay. Measured in `test_measured_latency_matches_constants`.
/// RNNoise: 480 priming + 480 overlap-add window.
const RNNOISE_LATENCY_SAMPLES: usize = 2 * FRAME_SIZE;

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
  /// RNNoise's own voice probability when it is the model, else by `in_net` (asleep too).
  vad: VoiceHysteresis,
  /// Same detector on the denoised output: keys are gone there, so it keeps hearing a voice the
  /// raw-input one loses under typing (private voice, keys at -10 dB SNR: RNNoise's probability
  /// on soft syllables fell from ~0.95 to <0.1 and the gate cut 18-31 % of them).
  vad_out: VoiceHysteresis,
  /// RNNoise run only for its voice probability: on the raw input while a heavy model is selected,
  /// asleep or not (RNNoise 0.2, Full; ~10x the 2017 pass, ~2.8 % of a core). Its probability describes the
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
  /// Frames in a row `in_net` rated above `LEVEL_PROB` (the leveler's key, see `level_key`).
  level_run: u32,
  /// A full voice, not a whisper, was heard in the last block: what the leveler and the automatic
  /// input gain learn from. With RNNoise as the model it is `heard_voice` (its own probability).
  heard_level_voice: bool,
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
  /// Samples since the raw-input detectors last heard a voice (`RESCUE_IDLE_SAMPLES`).
  no_voice_samples: usize,
  /// RNNoise ran in the last block; the rescue starting it again waits `rescue_warm_left`.
  rnnoise_ran: bool,
  rescue_warm_left: usize,
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
      level_run: 0,
      heard_level_voice: false,
      loading: None,
      mute_left: 0,
      no_voice_samples: 0,
      rnnoise_ran: false,
      rescue_warm_left: 0,
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

  /// A full voice (not a whisper) was heard in the block just processed: what the leveler and the
  /// automatic input gain learn from. Same timing as `heard_voice`.
  pub fn heard_level_voice(&self) -> bool {
    self.heard_level_voice
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
      let (vad, in_prob, level_run) = (&mut self.vad, &mut self.in_prob, &mut self.level_run);
      let (mut voice, mut level_voice) = (false, false);
      let (open, keep) = self.in_net.thresholds();
      self.in_net.process(samples, |p| {
        *in_prob = in_prob.max(p);
        voice |= vad.frame_at(p, open, keep);
        level_voice |= level_key(level_run, p);
      });
      self.heard_voice = voice;
      self.heard_level_voice = level_voice;
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
    let rescue_on = heavy_on
      && takeover.is_none()
      && active == DenoiseModel::DeepFilterNet
      && !rescue_off
      && self.no_voice_samples < RESCUE_IDLE_SAMPLES;
    let rnnoise_on = !heavy_on || takeover.is_some() || rescue_on;
    if rescue_on && !self.rnnoise_ran {
      // RNNoise's buffers and the delay hold audio from before the pause, its state is stale.
      self.rnnoise.restart();
      self.sleep.delay.clear();
      self.rescue_warm_left = STAND_IN_WARMUP_SAMPLES;
    }
    self.rnnoise_ran = rnnoise_on;
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
      level_run,
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
      no_voice_samples,
      rescue_warm_left,
      ..
    } = self;
    let (mut voice, mut voice_out, mut voice_soft, mut level_voice) = (false, false, false, false);
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
    // RNNoise's own voice probability when it is the model, else `in_net` (RNNoise 0.2), also
    // while RNNoise stands in: the 2017 net rated room noise as speech that `in_net` rated 0.00,
    // and woke the model 30 ms after it fell asleep (tester 2026-11-16: hiss every ~2 s).
    if heavy {
      let (open, keep) = in_net.thresholds();
      in_net.process(samples, |p| {
        *in_prob = in_prob.max(p);
        voice |= vad.frame_at(p, open, keep);
        level_voice |= level_key(level_run, p);
      });
    }
    *no_voice_samples = if voice || voice_soft { 0 } else { no_voice_samples.saturating_add(n) };
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
        if !heavy {
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
      let warm = *rescue_warm_left == 0;
      *rescue_warm_left = rescue_warm_left.saturating_sub(n);
      *rescue_prob = rescue.process(samples, stand, soft_voice_now && warm);
    } else {
      rescue.reset();
    }
    // The output detector, in step with the output (its hold needs no latency on top). Not while
    // a heavy model sleeps: it would hear the keys RNNoise leaves in (DFN3, loud typing: gate
    // open 25 % instead of 1 %); the input detector alone wakes the model. Nor while it wakes:
    // the output is still RNNoise's for a model latency, and its noise opened the gate for 200 ms
    // on every wake in a pause (tester 2026-11-16: hiss every ~7 s, heavy models only).
    let waking = takeover.is_some_and(|t| !t.to_sleep);
    if gate_on && (heavy_on || !heavy) && !waking {
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
    self.heard_level_voice = if heavy { level_voice } else { voice };
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
    self.rnnoise_ran = false;
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
    self.level_run = 0;
    self.vad_out = VoiceHysteresis::new_output();
    self.delay.clear();
    self.bypassed = false;
    self.no_voice_samples = 0;
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

  /// Fresh engines for a model switch: sleep and the click rescue cleared (its hold is the old
  /// engine's, or from before a bypass), nothing left to silence, RNNoise's network state new, the
  /// heavy models' buffers emptied.
  fn reset_engines(&mut self) {
    self.rnnoise_ran = false;
    self.sleep.clear();
    self.rescue.reset();
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

/// Strength -> share of the dry signal the heavy models mix back (None = unlimited).
fn atten_lim(strength: f32) -> Option<f32> {
  if strength >= 0.999 {
    None
  } else {
    let db = DF_MIN_ATTEN_DB + strength * (DF_MAX_ATTEN_DB - DF_MIN_ATTEN_DB);
    Some(10f32.powf(-db / 20.0))
  }
}

fn lookahead_samples(model: DenoiseModel) -> usize {
  match model {
    DenoiseModel::RnNoise | DenoiseModel::Unknown(_) => RNNOISE_LOOKAHEAD_SAMPLES,
    DenoiseModel::DeepFilterNet => DF_LOOKAHEAD_SAMPLES,
    DenoiseModel::DpdfNet => DPDF_LOOKAHEAD_SAMPLES,
  }
}

/// One `in_net` frame into the leveler's key: true once `LEVEL_MIN_FRAMES` frames in a row are sure.
fn level_key(run: &mut u32, prob: f32) -> bool {
  *run = if prob > LEVEL_PROB { run.saturating_add(1) } else { 0 };
  *run >= LEVEL_MIN_FRAMES
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
