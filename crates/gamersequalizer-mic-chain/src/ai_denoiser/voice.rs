//! Voice detection: RNNoise run for its voice probability, and the hysteresis on top.

use super::*;

/// A frame is speech above this RNNoise voice probability. Measured on mechanical keys + mouse
/// clicks without speech: never fired at 0.5 (DeepFilterNet's local SNR fired on 9-31 %).
pub(super) const VOICE_PROB: f32 = 0.5;
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
pub(super) const IN_NET_VOICE_PROB: f32 = 0.7;
const IN_NET_KEEP_PROB: f32 = 0.2;

/// The second detector, on the denoised output, opens on one frame above this and stays open
/// longer: keys are mostly gone there, and a cut voice is worse than a few keys let through.
/// Measured on a private voice under its own keyboard (`private_report`): phrase onsets after
/// a pause scored 0.6-0.9 there for one frame but < 0.1 on the raw input; soft syllables cut
/// by the gate over no gate 6-15 -> 1-4 %. Cost: typing alone (ESC-50 keys, -20 dBFS) keeps
/// it open most of the time with RNNoise/DFN3, so only the floor below stops those keys.
const OUT_VOICE_PROB: f32 = 0.3;
const OUT_VOICE_MIN_FRAMES: u32 = 1;
const OUT_VOICE_KEEP_PROB: f32 = 0.02;
const OUT_VOICE_ACTIVE_FRAMES: u32 = 50;

/// RNNoise run only for its voice probability (see `vad`, `in_net`, `out_net`).
pub(super) struct VoiceNet {
  pub(super) state: VoiceModel,
  pub(super) input: FrameInput,
  frame: [f32; FRAME_SIZE],
  scratch: [f32; FRAME_SIZE],
}

/// The 2017 network (`out_net`, `ClickRescue`) or RNNoise 0.2 (`in_net`).
pub(super) enum VoiceModel {
  Old(Box<DenoiseState<'static>>),
  New(Box<DenoiseState2>),
}

/// Speech decision from RNNoise's per-frame voice probability, with hysteresis. The RNNoise
/// denoiser hears the raw input anyway, so its own probability keys `vad` (a second pass on the
/// same input cost ~0.26 % of a core). Behind a heavy model `vad` needs its own pass: keying it
/// on the heavy model's output instead lost laughter (DFN3 removes part of it; the gate then
/// closed on it, -11 dB on the private scenario take).
pub(super) struct VoiceHysteresis {
  /// Consecutive speech frames so far.
  pub(super) run: u32,
  /// Frames left in which a weaker frame (above `keep_prob`) still counts as speech.
  pub(super) active_left: u32,
  pub(super) keep_prob: f32,
  pub(super) active_frames: u32,
  open_prob: f32,
  min_frames: u32,
}

impl VoiceNet {
  /// The 2017 network (`out_net`, `ClickRescue`).
  pub(super) fn new() -> Self {
    Self::with_model(VoiceModel::Old(DenoiseState::new()))
  }

  /// `in_net`: RNNoise 0.2 (Full). If its embedded weights fail to load (never seen; a corrupt
  /// build), the 2017 network instead, so the chain always has a detector.
  pub(super) fn new_input() -> Self {
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
  pub(super) fn thresholds(&self) -> (f32, f32) {
    match self.state {
      VoiceModel::Old(_) => (VOICE_PROB, VOICE_KEEP_PROB),
      VoiceModel::New(_) => (IN_NET_VOICE_PROB, IN_NET_KEEP_PROB),
    }
  }

  /// Calls `on_prob` with the voice probability of each frame completed in this block
  /// (RNNoise 0.2: of the frame before it, see `in_net`).
  pub(super) fn process(&mut self, samples: &[f32], mut on_prob: impl FnMut(f32)) {
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
  pub(super) fn reset(&mut self) {
    match &mut self.state {
      VoiceModel::Old(state) => *state = DenoiseState::new(),
      VoiceModel::New(state) => state.reset(),
    }
    self.input.clear();
  }
}

impl VoiceHysteresis {
  pub(super) fn new() -> Self {
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
  pub(super) fn new_output() -> Self {
    Self {
      open_prob: OUT_VOICE_PROB,
      min_frames: OUT_VOICE_MIN_FRAMES,
      keep_prob: OUT_VOICE_KEEP_PROB,
      active_frames: OUT_VOICE_ACTIVE_FRAMES,
      ..Self::new()
    }
  }

  /// One frame's voice probability -> is it speech.
  pub(super) fn frame(&mut self, prob: f32) -> bool {
    self.frame_at(prob, self.open_prob, self.keep_prob)
  }

  /// `frame` with the thresholds of the network that scored `prob` (`VoiceNet::thresholds`).
  pub(super) fn frame_at(&mut self, prob: f32, open_prob: f32, keep_prob: f32) -> bool {
    self.run = if prob > open_prob { self.run.saturating_add(1) } else { 0 };
    // Hysteresis: opening needs a clear syllable; once talking, a soft one keeps it open.
    let voice = self.run >= self.min_frames || (self.active_left > 0 && prob > keep_prob);
    self.active_left = if voice { self.active_frames } else { self.active_left.saturating_sub(1) };
    voice
  }
}
