//! DeepFilterNet 3 as the denoiser.

use super::*;

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

pub(super) struct DeepFilterEngine {
  model: DfTract,
  input: FrameInput,
  output: FrameOutput,
  in_frame: Array2<f32>,
  out_frame: Array2<f32>,
  error_logged: bool,
}

impl DeepFilterEngine {
  pub(super) fn load() -> Result<Self, String> {
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

  pub(super) fn process(&mut self, samples: &mut [f32], strength: f32, pre_gain: f32) {
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

  pub(super) fn reset(&mut self) {
    self.input.clear();
    self.output.clear();
  }
}
