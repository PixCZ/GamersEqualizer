//! Click rescue: RNNoise fills in where DeepFilterNet 3 cut a voice with a click.

use super::*;

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
/// core), only while the raw input had a voice in the last 2 s (`RESCUE_IDLE_SAMPLES`); in
/// rescued frames the click comes through as RNNoise leaves it (voice over keys).
pub(super) struct ClickRescue {
  /// Voice detector on DFN3's output (before the rescue).
  net: VoiceNet,
  pub(super) hold_left: usize,
  /// Energy envelopes of the DFN3 output and RNNoise's.
  env_dfn: f32,
  env_rnn: f32,
  /// RNNoise's share of the output.
  pub(super) mix: f32,
}

impl ClickRescue {
  pub(super) fn new() -> Self {
    Self { net: VoiceNet::new(), hold_left: 0, env_dfn: 0.0, env_rnn: 0.0, mix: 0.0 }
  }

  /// `out`: DFN3 output, in place; `rnn`: RNNoise's output of the same samples; `voice_now`:
  /// Silero hears a voice in the raw input around these samples (see `RESCUE_SOFT_PROB`). Returns the
  /// detector's highest voice probability in this block (-1: no frame completed).
  pub(super) fn process(&mut self, out: &mut [f32], rnn: &[f32], voice_now: bool) -> f32 {
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
  pub(super) fn reset(&mut self) {
    self.net.input.clear();
    (self.hold_left, self.env_dfn, self.env_rnn, self.mix) = (0, 0.0, 0.0, 0.0);
  }
}
