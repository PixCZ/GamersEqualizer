//! The frame engines (RNNoise, DeepFilterNet, DPDFNet, the voice detectors) work on fixed frames;
//! the host's blocks have any size. Input is collected into frames here, and output handed back
//! block by block. Real-time safe: the buffers are allocated up front and only grow for a block
//! longer than they were made for.

/// RNNoise (both networks) is trained on i16-range samples, not [-1.0, 1.0].
pub(crate) const I16_SCALE: f32 = 32768.0;

/// Input collected into windows of `window` samples, one every `hop`. `window == hop`: plain
/// frames; a longer window starts with the previous one's last `window - hop` samples (zeros at
/// the start), so a frame runs as soon as one hop has arrived.
pub(crate) struct FrameInput {
  buf: Vec<f32>,
  window: usize,
  hop: usize,
}

/// Output primed with one hop of silence, so every block gets as many samples out as it put in.
pub(crate) struct FrameOutput {
  buf: Vec<f32>,
  hop: usize,
}

impl FrameInput {
  pub(crate) fn new(window: usize, hop: usize) -> Self {
    let mut input = Self { buf: Vec::with_capacity(window + hop * 4), window, hop };
    input.clear();
    input
  }

  pub(crate) fn push(&mut self, samples: &[f32]) {
    self.buf.extend_from_slice(samples);
  }

  /// Like `push`, every sample times `gain`.
  pub(crate) fn push_scaled(&mut self, samples: &[f32], gain: f32) {
    self.buf.extend(samples.iter().map(|s| s * gain));
  }

  /// The next whole window, if it is in; `advance` moves on to the one after it.
  pub(crate) fn window(&self) -> Option<&[f32]> {
    self.buf.get(..self.window)
  }

  pub(crate) fn advance(&mut self) {
    self.buf.drain(..self.hop.min(self.buf.len()));
  }

  /// Nothing buffered (a longer window's overlap back to zeros).
  pub(crate) fn clear(&mut self) {
    self.buf.clear();
    self.buf.resize(self.window - self.hop, 0.0);
  }
}

impl FrameOutput {
  pub(crate) fn new(hop: usize) -> Self {
    let mut output = Self { buf: Vec::with_capacity(hop * 4), hop };
    output.clear();
    output
  }

  pub(crate) fn push(&mut self, sample: f32) {
    self.buf.push(sample);
  }

  pub(crate) fn extend(&mut self, samples: impl IntoIterator<Item = f32>) {
    self.buf.extend(samples);
  }

  /// Moves finished samples into `samples`; the primed hop guarantees enough are there.
  pub(crate) fn drain_into(&mut self, samples: &mut [f32]) {
    let n = samples.len().min(self.buf.len());
    for (dst, src) in samples.iter_mut().zip(self.buf.drain(..n)) {
      *dst = src;
    }
    // Only reachable for a block longer than the priming: silence, never the raw input.
    samples.iter_mut().skip(n).for_each(|s| *s = 0.0);
  }

  /// Nothing buffered but the primed hop of silence.
  pub(crate) fn clear(&mut self) {
    self.buf.clear();
    self.buf.resize(self.hop, 0.0);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Any block size gets as many samples out as it put in, one hop later.
  #[test]
  fn test_frames_keep_length_and_delay_one_hop() {
    let (mut input, mut output) = (FrameInput::new(4, 4), FrameOutput::new(4));
    let signal: Vec<f32> = (1..=20).map(|i| i as f32).collect();
    let mut out = Vec::new();
    for block in signal.chunks(3) {
      input.push(block);
      while let Some(frame) = input.window() {
        output.extend(frame.iter().copied());
        input.advance();
      }
      let mut done = vec![-1.0; block.len()];
      output.drain_into(&mut done);
      out.extend(done);
    }
    assert_eq!(out.len(), signal.len());
    assert_eq!(&out[..4], &[0.0; 4]);
    assert_eq!(&out[4..], &signal[..16]);
  }

  /// A window longer than the hop starts with the last one's tail, zeros at the start.
  #[test]
  fn test_overlapping_windows() {
    let mut input = FrameInput::new(4, 2);
    input.push(&[1.0, 2.0, 3.0, 4.0]);
    let mut windows = Vec::new();
    while let Some(w) = input.window() {
      windows.push(w.to_vec());
      input.advance();
    }
    assert_eq!(windows, [vec![0.0, 0.0, 1.0, 2.0], vec![1.0, 2.0, 3.0, 4.0]]);
    input.clear();
    assert_eq!(input.window(), None);
  }
}
