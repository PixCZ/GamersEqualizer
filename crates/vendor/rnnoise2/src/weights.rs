//! Weight blob parser: the upstream record format (`nnet.h` `WeightHead`, `parse_lpcnet_weights.c`).
//!
//! The blob is parsed once at construction. Float arrays are copied into owned tables; int8
//! weights stay borrowed from the embedded `'static` blob. Every size the network relies on is
//! checked here, so the per-frame code never sees a short table.

use std::fmt;

use crate::bands::NB_BANDS;
use crate::kernel::Int8Matrix;
use crate::nnet::{Linear, RnnModel, Weights, CONV1_IN, CONV1_OUT, CONV2_OUT, DENSE_IN, GRU_SIZE, KERNEL};

const HEADER_SIZE: usize = 64;
const NAME_SIZE: usize = 44;
/// One sparse block: 8 outputs x 4 inputs of int8.
const SPARSE_BLOCK_SIZE: usize = 32;

/// Why a weight blob was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsError {
  /// A record header at this byte offset is malformed or runs past the end of the blob.
  BadRecord { offset: usize },
  /// A required array is not in the blob.
  Missing { name: &'static str },
  /// An array has the wrong payload size in bytes.
  WrongSize { name: &'static str, expected: usize, actual: usize },
  /// A sparse index array is malformed (bad count, unaligned or out-of-range offset).
  BadIndex { name: &'static str },
}

impl fmt::Display for WeightsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      WeightsError::BadRecord { offset } => write!(f, "malformed weight record at byte {offset}"),
      WeightsError::Missing { name } => write!(f, "weight array `{name}` missing"),
      WeightsError::WrongSize { name, expected, actual } => {
        write!(f, "weight array `{name}` has {actual} bytes, expected {expected}")
      }
      WeightsError::BadIndex { name } => write!(f, "sparse index array `{name}` is malformed"),
    }
  }
}

impl std::error::Error for WeightsError {}

#[derive(Clone, Copy)]
struct Record<'a> {
  name: &'a [u8],
  data: &'a [u8],
}

/// Parses the whole blob into a model (`parse_weights` + `init_rnnoise`).
pub(crate) fn load_model(blob: &'static [u8]) -> Result<RnnModel, WeightsError> {
  validate(blob)?;
  // `linear_init` for an f32 dense layer.
  macro_rules! dense {
    ($p:literal, $nb_in:expr, $nb_out:expr) => {
      Linear::new(
        $nb_in,
        $nb_out,
        Weights::Float(floats(blob, concat!($p, "_weights_float"), $nb_in * $nb_out)?),
        None,
        floats(blob, concat!($p, "_bias"), $nb_out)?,
        None,
      )
    };
  }
  // `linear_init` for an int8 layer; `subias` is only used by upstream's x86 AVX path, but
  // `linear_init` requires it, so it is checked and dropped.
  macro_rules! int8 {
    ($p:literal, $weights:expr, $diag:expr, $nb_in:expr, $nb_out:expr) => {{
      floats(blob, concat!($p, "_subias"), $nb_out)?;
      Linear::new(
        $nb_in,
        $nb_out,
        $weights,
        Some(floats(blob, concat!($p, "_scale"), $nb_out)?),
        floats(blob, concat!($p, "_bias"), $nb_out)?,
        $diag,
      )
    }};
  }
  macro_rules! sparse {
    ($p:literal, $diag:expr) => {{
      let name = concat!($p, "_weights_idx");
      let (idx, blocks) = sparse_idx(blob, name, GRU_SIZE, 3 * GRU_SIZE)?;
      let weights = find_sized(blob, concat!($p, "_weights_int8"), SPARSE_BLOCK_SIZE * blocks)?;
      // Also rejects more blocks per output group than input groups (the kernel's exactness bound).
      let matrix = Int8Matrix::sparse(weights, &idx, GRU_SIZE, 3 * GRU_SIZE).ok_or(WeightsError::BadIndex { name })?;
      int8!($p, Weights::Int8(matrix), $diag, GRU_SIZE, 3 * GRU_SIZE)
    }};
  }
  macro_rules! gru {
    ($input:literal, $recurrent:literal) => {
      (
        sparse!($input, None),
        sparse!($recurrent, Some(floats(blob, concat!($recurrent, "_weights_diag"), 3 * GRU_SIZE)?)),
      )
    };
  }
  let conv2_size = KERNEL * CONV1_OUT * CONV2_OUT;
  let conv2_weights = find_sized(blob, "conv2_weights_int8", conv2_size)?;
  // Cannot fail: the shape is valid and the size was just checked.
  let conv2_weights = Int8Matrix::dense(conv2_weights, KERNEL * CONV1_OUT, CONV2_OUT)
    .ok_or(WeightsError::WrongSize { name: "conv2_weights_int8", expected: conv2_size, actual: conv2_weights.len() })?;
  let conv2_weights = Weights::Int8(conv2_weights);
  Ok(RnnModel {
    conv1: dense!("conv1", KERNEL * CONV1_IN, CONV1_OUT),
    conv2: int8!("conv2", conv2_weights, None, KERNEL * CONV1_OUT, CONV2_OUT),
    gru1: gru!("gru1_input", "gru1_recurrent"),
    gru2: gru!("gru2_input", "gru2_recurrent"),
    gru3: gru!("gru3_input", "gru3_recurrent"),
    dense_out: dense!("dense_out", DENSE_IN, NB_BANDS),
    vad_dense: dense!("vad_dense", DENSE_IN, 1),
  })
}

/// Walks every record like `parse_weights`: any malformed record (or one with `size == 0`) fails
/// the whole blob.
fn validate(blob: &[u8]) -> Result<(), WeightsError> {
  records(blob).try_for_each(|r| r.map(|_| ()))
}

/// Iterates the records; yields an error (and stops) at the first malformed header.
fn records(blob: &[u8]) -> impl Iterator<Item = Result<Record<'_>, WeightsError>> {
  let mut offset = 0usize;
  let mut failed = false;
  std::iter::from_fn(move || {
    if failed || offset >= blob.len() {
      return None;
    }
    let record = parse_record(blob, offset);
    match record {
      Ok((_, next)) => offset = next,
      Err(_) => failed = true,
    }
    Some(record.map(|(record, _)| record))
  })
}

/// `parse_record`: checks the header at `offset`, returns the record and the next offset.
fn parse_record(blob: &[u8], offset: usize) -> Result<(Record<'_>, usize), WeightsError> {
  let bad = WeightsError::BadRecord { offset };
  let rest = blob.get(offset..).ok_or(bad.clone())?;
  let header = rest.get(..HEADER_SIZE).ok_or(bad.clone())?;
  let size = read_i32(header, 12).ok_or(bad.clone())?;
  let block_size = read_i32(header, 16).ok_or(bad.clone())?;
  let name = header.get(20..20 + NAME_SIZE).ok_or(bad.clone())?;
  let avail = rest.len() - HEADER_SIZE;
  // `size <= 0` covers both `size < 0` (parse_record) and `size == 0` (parse_weights).
  if size <= 0 || block_size < size || name.last() != Some(&0) {
    return Err(bad);
  }
  let (size, block_size) = (size as usize, block_size as usize);
  if block_size > avail {
    return Err(bad);
  }
  let data = rest.get(HEADER_SIZE..HEADER_SIZE + size).ok_or(bad)?;
  let name_len = name.iter().position(|&b| b == 0).unwrap_or(NAME_SIZE);
  let name = name.get(..name_len).unwrap_or_default();
  Ok((Record { name, data }, offset + HEADER_SIZE + block_size))
}

/// `find_array_entry`: the first record named `name`.
fn find<'a>(blob: &'a [u8], name: &'static str) -> Result<Record<'a>, WeightsError> {
  records(blob).filter_map(Result::ok).find(|r| r.name == name.as_bytes()).ok_or(WeightsError::Missing { name })
}

/// `find_array_check`: the first record named `name`, with exactly `size` payload bytes.
fn find_sized<'a>(blob: &'a [u8], name: &'static str, size: usize) -> Result<&'a [u8], WeightsError> {
  let record = find(blob, name)?;
  if record.data.len() != size {
    return Err(WeightsError::WrongSize { name, expected: size, actual: record.data.len() });
  }
  Ok(record.data)
}

fn floats(blob: &[u8], name: &'static str, count: usize) -> Result<Box<[f32]>, WeightsError> {
  let data = find_sized(blob, name, 4 * count)?;
  Ok(data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// `find_idx_check`: per group of 8 outputs a block count, then that many input offsets (each a
/// multiple of 4 with `pos + 3 < nb_in`); the groups must cover exactly `nb_out`. Returns the
/// decoded array and the total number of blocks.
fn sparse_idx(
  blob: &[u8],
  name: &'static str,
  nb_in: usize,
  nb_out: usize,
) -> Result<(Box<[u32]>, usize), WeightsError> {
  let record = find(blob, name)?;
  let bad = WeightsError::BadIndex { name };
  if record.data.len() % 4 != 0 {
    return Err(bad);
  }
  let values: Vec<i32> = record.data.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c)).collect();
  let mut it = values.iter();
  let (mut groups, mut total) = (0usize, 0usize);
  while let Some(&count) = it.next() {
    // Stricter than upstream: negative counts and offsets are rejected too.
    let count = usize::try_from(count).map_err(|_| bad.clone())?;
    for _ in 0..count {
      let pos = it.next().ok_or(bad.clone())?;
      let pos = usize::try_from(*pos).map_err(|_| bad.clone())?;
      if pos + 3 >= nb_in || pos % 4 != 0 {
        return Err(bad);
      }
    }
    groups += 1;
    total += count;
  }
  if groups * 8 != nb_out {
    return Err(bad);
  }
  Ok((values.iter().map(|&v| v as u32).collect(), total))
}

fn read_i32(bytes: &[u8], at: usize) -> Option<i32> {
  Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
  use super::*;
  use crate::Model;

  #[test]
  fn embedded_blobs_parse() {
    for model in [Model::Full, Model::Little] {
      load_model(model.blob()).expect("embedded blob");
    }
  }

  #[test]
  fn embedded_blob_hashes_match() {
    for (model, file) in [(Model::Full, "rnnoise_full.bin.sha256"), (Model::Little, "rnnoise_little.bin.sha256")] {
      let path = concat!(env!("CARGO_MANIFEST_DIR"), "/models/").to_string() + file;
      let expected = std::fs::read_to_string(path).unwrap();
      let expected = expected.split_whitespace().next().unwrap();
      assert_eq!(hex(&sha256(model.blob())), expected, "{file}");
    }
  }

  #[test]
  fn truncated_blob_is_rejected() {
    let blob = Model::Little.blob();
    for cut in [10, 64 + 100, blob.len() - 1] {
      let short: &'static [u8] = Box::leak(blob[..cut].to_vec().into_boxed_slice());
      assert!(matches!(load_model(short), Err(WeightsError::BadRecord { .. })), "cut {cut}");
    }
    // Cut exactly at a record boundary: every record is well-formed, the tail arrays are missing.
    let boundary = blob.len() - 128;
    let short: &'static [u8] = Box::leak(blob[..boundary].to_vec().into_boxed_slice());
    assert_eq!(load_model(short).err(), Some(WeightsError::Missing { name: "vad_dense_bias" }));
  }

  #[test]
  fn wrong_name_is_missing() {
    let mut blob = Model::Little.blob().to_vec();
    let at = record_offset(&blob, "conv1_bias");
    blob[at + 20] = b'X';
    let blob: &'static [u8] = Box::leak(blob.into_boxed_slice());
    assert_eq!(load_model(blob).err(), Some(WeightsError::Missing { name: "conv1_bias" }));
  }

  #[test]
  fn wrong_size_is_rejected() {
    // Shrink `conv1_bias` from 512 to 508 bytes (block size stays, so the walk still works).
    let mut blob = Model::Little.blob().to_vec();
    let at = record_offset(&blob, "conv1_bias");
    blob[at + 12..at + 16].copy_from_slice(&508i32.to_le_bytes());
    let blob: &'static [u8] = Box::leak(blob.into_boxed_slice());
    assert_eq!(
      load_model(blob).err(),
      Some(WeightsError::WrongSize { name: "conv1_bias", expected: 512, actual: 508 })
    );
  }

  #[test]
  fn bad_sparse_index_is_rejected() {
    // First offset of `gru1_input_weights_idx` made unaligned.
    let mut blob = Model::Little.blob().to_vec();
    let at = record_offset(&blob, "gru1_input_weights_idx") + 64 + 4;
    blob[at..at + 4].copy_from_slice(&1i32.to_le_bytes());
    let blob: &'static [u8] = Box::leak(blob.into_boxed_slice());
    assert_eq!(load_model(blob).err(), Some(WeightsError::BadIndex { name: "gru1_input_weights_idx" }));
  }

  #[test]
  fn zero_size_record_fails_the_blob() {
    let mut blob = Model::Little.blob().to_vec();
    let at = record_offset(&blob, "conv1_bias");
    blob[at + 12..at + 16].copy_from_slice(&0i32.to_le_bytes());
    assert!(matches!(validate(&blob), Err(WeightsError::BadRecord { .. })));
  }

  fn record_offset(blob: &[u8], name: &str) -> usize {
    let mut offset = 0;
    loop {
      let (record, next) = parse_record(blob, offset).unwrap();
      if record.name == name.as_bytes() {
        return offset;
      }
      offset = next;
    }
  }

  fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
  }

  /// Minimal SHA-256 (FIPS 180-4), test only, so the crate needs no crypto dependency.
  fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
      0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
      0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
      0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
      0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
      0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
      0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
      0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
      0xc67178f2,
    ];
    let mut h: [u32; 8] =
      [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
      msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
      let mut w = [0u32; 64];
      for (w, word) in w.iter_mut().zip(chunk.as_chunks::<4>().0) {
        *w = u32::from_be_bytes(*word);
      }
      for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
      }
      let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
      for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
      }
      for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(y);
      }
    }
    let mut out = [0u8; 32];
    for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
      o.copy_from_slice(&x.to_be_bytes());
    }
    out
  }
}
