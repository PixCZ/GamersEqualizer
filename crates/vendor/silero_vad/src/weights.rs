//! The embedded weight blob (written by `tools/convert_weights.py`): magic, then per tensor a
//! u32 count and that many little-endian f32, in the order of `Weights::parse`.

use std::fmt;

pub const BINS: usize = 129;
pub const FFT: usize = 256;
pub const HIDDEN: usize = 128;
/// (inputs, outputs, stride) of the four encoder convolutions (kernel 3, padding 1).
pub const CONVS: [(usize, usize, usize); 4] = [(BINS, 128, 1), (128, 64, 2), (64, 64, 2), (64, HIDDEN, 1)];
const KERNEL: usize = 3;
const MAGIC: &[u8; 8] = b"SVAD6\0\0\0";

/// The weight blob is not what this code expects (a corrupt build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsError {
  BadMagic,
  Truncated { tensor: &'static str },
  WrongSize { tensor: &'static str, expected: usize, found: usize },
  TrailingBytes(usize),
}

impl fmt::Display for WeightsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::BadMagic => write!(f, "weight blob: wrong magic"),
      Self::Truncated { tensor } => write!(f, "weight blob: ends inside {tensor}"),
      Self::WrongSize { tensor, expected, found } => {
        write!(f, "weight blob: {tensor} has {found} values, expected {expected}")
      }
      Self::TrailingBytes(n) => write!(f, "weight blob: {n} bytes after the last tensor"),
    }
  }
}

impl std::error::Error for WeightsError {}

pub struct Conv {
  /// `[output][tap][input]`, so one tap of one output is a contiguous row.
  pub weight: Vec<f32>,
  pub bias: Vec<f32>,
  pub inputs: usize,
  pub stride: usize,
}

pub struct Weights {
  /// STFT as a filter bank: `BINS` real rows, then `BINS` imaginary rows, `FFT` taps each.
  pub stft: Vec<f32>,
  pub convs: [Conv; 4],
  /// LSTM cell, gates in PyTorch's order (input, forget, cell, output): `[4 * HIDDEN][HIDDEN]`.
  pub lstm_input: Vec<f32>,
  pub lstm_hidden: Vec<f32>,
  /// Both biases summed.
  pub lstm_bias: Vec<f32>,
  pub out_weight: Vec<f32>,
  pub out_bias: f32,
}

impl Weights {
  pub fn parse(blob: &[u8]) -> Result<Self, WeightsError> {
    let mut rest = blob.strip_prefix(MAGIC.as_slice()).ok_or(WeightsError::BadMagic)?;
    let mut tensor = |name: &'static str, expected: usize| -> Result<Vec<f32>, WeightsError> {
      let (count, tail) = rest.split_first_chunk::<4>().ok_or(WeightsError::Truncated { tensor: name })?;
      let found = u32::from_le_bytes(*count) as usize;
      if found != expected {
        return Err(WeightsError::WrongSize { tensor: name, expected, found });
      }
      let (data, tail) = tail.split_at_checked(found * 4).ok_or(WeightsError::Truncated { tensor: name })?;
      rest = tail;
      Ok(data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
    };
    let stft = tensor("stft", 2 * BINS * FFT)?;
    let mut conv =
      |name: &'static str, (inputs, outputs, stride): (usize, usize, usize)| -> Result<Conv, WeightsError> {
        // Upstream layout [output][input][tap] -> [output][tap][input].
        let raw = tensor(name, outputs * inputs * KERNEL)?;
        let mut weight = vec![0.0; raw.len()];
        for (dst, src) in weight.chunks_exact_mut(KERNEL * inputs).zip(raw.chunks_exact(inputs * KERNEL)) {
          for (i, taps) in src.as_chunks::<KERNEL>().0.iter().enumerate() {
            for (k, &w) in taps.iter().enumerate() {
              if let Some(slot) = dst.get_mut(k * inputs + i) {
                *slot = w;
              }
            }
          }
        }
        Ok(Conv { weight, bias: tensor(name, outputs)?, inputs, stride })
      };
    let [c0, c1, c2, c3] = CONVS;
    let convs = [conv("conv 1", c0)?, conv("conv 2", c1)?, conv("conv 3", c2)?, conv("conv 4", c3)?];
    let lstm_input = tensor("lstm input weights", 4 * HIDDEN * HIDDEN)?;
    let lstm_hidden = tensor("lstm hidden weights", 4 * HIDDEN * HIDDEN)?;
    let bias_input = tensor("lstm input bias", 4 * HIDDEN)?;
    let bias_hidden = tensor("lstm hidden bias", 4 * HIDDEN)?;
    let lstm_bias = bias_input.iter().zip(&bias_hidden).map(|(a, b)| a + b).collect();
    let out_weight = tensor("output weights", HIDDEN)?;
    let out_bias = tensor("output bias", 1)?.first().copied().unwrap_or_default();
    if !rest.is_empty() {
      return Err(WeightsError::TrailingBytes(rest.len()));
    }
    Ok(Self { stft, convs, lstm_input, lstm_hidden, lstm_bias, out_weight, out_bias })
  }
}
