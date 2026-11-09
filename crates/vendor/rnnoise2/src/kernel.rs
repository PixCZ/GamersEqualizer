//! The int8 matrix-vector product of `cgemv8x4` / `sparse_cgemv8x4` (8x4 blocks), with SIMD paths
//! picked once at construction: AVX2 on x86_64, `dotprod` on aarch64, else the scalar reference.
//!
//! Exactness: every path returns the exact integer sum per output, so all are bit-identical.
//! Weights and quantized inputs are `i8`, so one product is at most 128 * 128 = 16384 in magnitude
//! (in practice inputs are -127..=127, see `nnet::quantize`), and one output sums at most
//! `MAX_GROUPS * 4 = 384` products (`Int8Matrix::new` rejects more blocks per output group than
//! input groups): every partial sum is below 384 * 16384 = 6 291 456 < 2^24, in any order.
//! The C (and the previous port) adds each block's 4-term integer sum into an f32 starting at 0;
//! f32 represents every integer below 2^24 exactly, so each of those additions is exact and the
//! f32 result equals `sum as f32` of the i32 accumulation here, whatever the summation order.
//! AVX2 uses `madd_epi16` (i16 products added pairwise in i32, exact), not `maddubs_epi16`
//! (saturates to i16), which is why upstream's AVX2 build differs from its generic C.

#![allow(
  unsafe_code,
  reason = "SIMD intrinsics need unsafe for raw-pointer loads/stores and calls into target_feature fns"
)]

/// Input groups of 4 (`MAX_INPUTS / 4` of `nnet.rs`: 384 inputs).
pub(crate) const MAX_GROUPS: usize = 96;
/// Largest output count (a GRU's 3 x 384 gates).
pub(crate) const MAX_OUTPUTS: usize = 1152;
/// One block: 8 outputs x 4 inputs of int8, `w[k * 4 + c]` for output `k`, input `c`.
const BLOCK_SIZE: usize = 32;
const _: () = assert!(MAX_GROUPS <= u8::MAX as usize, "column indices are stored as u8");

/// Quantized input, in groups of 4.
pub(crate) type Input = [[i8; 4]; MAX_GROUPS];
/// Integer result per output.
pub(crate) type Output = [i32; MAX_OUTPUTS];

/// Which implementation runs. Only `detect` (after the CPU check) and `SCALAR` create one, which
/// is what makes calling the `target_feature` functions sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Kernel(Path);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
  Scalar,
  #[cfg(target_arch = "x86_64")]
  Avx2,
  #[cfg(target_arch = "aarch64")]
  Dotprod,
}

/// An int8 weight matrix as a flat block list: blocks in weight order, the input group of each,
/// and the block count of each output group of 8. Validated once in `new`, so the kernels index
/// only within bounds.
pub(crate) struct Int8Matrix {
  weights: &'static [u8],
  counts: Box<[u8]>,
  cols: Box<[u8]>,
  kernel: Kernel,
}

impl Kernel {
  pub(crate) const SCALAR: Kernel = Kernel(Path::Scalar);

  /// The fastest path this CPU supports (a cached feature check; call it at construction).
  pub(crate) fn detect() -> Kernel {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
      return Kernel(Path::Avx2);
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("dotprod") {
      return Kernel(Path::Dotprod);
    }
    Kernel::SCALAR
  }
}

impl Int8Matrix {
  /// `cgemv8x4` layout: all blocks, by output group, then input group.
  pub(crate) fn dense(weights: &'static [u8], nb_inputs: usize, nb_outputs: usize) -> Option<Self> {
    let groups = u8::try_from(nb_inputs / 4).ok()?;
    let counts = vec![groups; nb_outputs / 8];
    let cols = (0..nb_outputs / 8).flat_map(|_| 0..groups).collect();
    Self::new(weights, counts.into(), cols, nb_inputs, nb_outputs)
  }

  /// `sparse_cgemv8x4` layout: `idx` lists per output group a block count, then the input offset
  /// (a multiple of 4) of each block.
  pub(crate) fn sparse(weights: &'static [u8], idx: &[u32], nb_inputs: usize, nb_outputs: usize) -> Option<Self> {
    let (mut counts, mut cols) = (Vec::new(), Vec::new());
    let mut it = idx.iter();
    while let Some(&count) = it.next() {
      counts.push(u8::try_from(count).ok()?);
      for _ in 0..count {
        let pos = *it.next()?;
        if !pos.is_multiple_of(4) {
          return None;
        }
        cols.push(u8::try_from(pos / 4).ok()?);
      }
    }
    Self::new(weights, counts.into(), cols.into(), nb_inputs, nb_outputs)
  }

  fn new(weights: &'static [u8], counts: Box<[u8]>, cols: Box<[u8]>, nb_in: usize, nb_out: usize) -> Option<Self> {
    let groups = nb_in / 4;
    let valid = nb_in.is_multiple_of(4)
      && groups <= MAX_GROUPS
      && nb_out.is_multiple_of(8)
      && nb_out <= MAX_OUTPUTS
      && counts.len() * 8 == nb_out
      // The exactness bound in the module doc.
      && counts.iter().all(|&c| usize::from(c) <= groups)
      && counts.iter().map(|&c| usize::from(c)).sum::<usize>() == cols.len()
      && cols.iter().all(|&c| usize::from(c) < groups)
      && weights.len() >= cols.len() * BLOCK_SIZE;
    valid.then(|| Int8Matrix { weights, counts, cols, kernel: Kernel::detect() })
  }

  /// `out[i] = sum_j w[i][j] * x[j]` for every output of the matrix; the rest of `out` is untouched.
  pub(crate) fn gemv(&self, out: &mut Output, x: &Input) {
    self.gemv_with(self.kernel, out, x);
  }

  fn gemv_with(&self, kernel: Kernel, out: &mut Output, x: &Input) {
    match kernel.0 {
      Path::Scalar => self.gemv_scalar(out, x),
      // SAFETY: `Path::Avx2` is only created by `Kernel::detect` after `is_x86_feature_detected!("avx2")`.
      #[cfg(target_arch = "x86_64")]
      Path::Avx2 => unsafe { avx2::gemv(self, out, x) },
      // SAFETY: `Path::Dotprod` is only created by `Kernel::detect` after the `dotprod` check.
      #[cfg(target_arch = "aarch64")]
      Path::Dotprod => unsafe { dotprod::gemv(self, out, x) },
    }
  }

  /// The reference: per block `y[k] += w[4k]*x0 + w[4k+1]*x1 + w[4k+2]*x2 + w[4k+3]*x3`.
  fn gemv_scalar(&self, out: &mut Output, x: &Input) {
    for (y, (blocks, cols)) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.groups()) {
      let mut acc = [0i32; 8];
      for (block, &col) in blocks.iter().zip(cols) {
        let [x0, x1, x2, x3] = x.get(usize::from(col)).copied().unwrap_or_default().map(i32::from);
        for (a, w) in acc.iter_mut().zip(block.as_chunks::<4>().0) {
          let [w0, w1, w2, w3] = w.map(|b| i32::from(b as i8));
          *a = a.wrapping_add(w0 * x0 + w1 * x1 + w2 * x2 + w3 * x3);
        }
      }
      *y = acc;
    }
  }

  /// Per output group of 8: its blocks and their input groups.
  fn groups(&self) -> impl Iterator<Item = (&[[u8; BLOCK_SIZE]], &[u8])> {
    let (mut blocks, mut cols) = (self.weights.as_chunks::<BLOCK_SIZE>().0, &*self.cols);
    self.counts.iter().map_while(move |&count| {
      let (b, rest_b) = blocks.split_at_checked(usize::from(count))?;
      let (c, rest_c) = cols.split_at_checked(usize::from(count))?;
      (blocks, cols) = (rest_b, rest_c);
      Some((b, c))
    })
  }
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
  use core::arch::x86_64::{
    __m128i, __m256i, _mm256_add_epi32, _mm256_cvtepi8_epi16, _mm256_hadd_epi32, _mm256_madd_epi16,
    _mm256_permute4x64_epi64, _mm256_set1_epi64x, _mm256_setzero_si256, _mm256_storeu_si256, _mm_loadu_si128,
  };

  use super::{Input, Int8Matrix, Output};

  /// Exact i16 path (not `maddubs`, which saturates): weights and inputs widened to i16, `madd`
  /// sums adjacent products into i32. Per block, `lo` collects outputs 0..4, `hi` outputs 4..8,
  /// two i32 partial sums per output; `hadd` + one permute fold them per output group.
  #[target_feature(enable = "avx2")]
  pub(super) fn gemv(m: &Int8Matrix, out: &mut Output, x: &Input) {
    // Each input group as four i16 in one i64, broadcast to all four 64-bit lanes per block;
    // 256 entries so a u8 column needs no bounds check.
    let mut xw = [0i64; 256];
    for (w, x) in xw.iter_mut().zip(x) {
      let [a, b, c, d] = x.map(|v| u64::from(i16::from(v) as u16));
      *w = (a | b << 16 | c << 32 | d << 48) as i64;
    }
    for (y, (blocks, cols)) in out.as_chunks_mut::<8>().0.iter_mut().zip(m.groups()) {
      let mut lo = _mm256_setzero_si256();
      let mut hi = _mm256_setzero_si256();
      for (block, &col) in blocks.iter().zip(cols) {
        let xv = _mm256_set1_epi64x(xw.get(usize::from(col)).copied().unwrap_or(0));
        let ptr = block.as_ptr().cast::<__m128i>();
        // SAFETY: `block` is a `&[u8; 32]`: both 16-byte unaligned loads (`loadu`) are in bounds.
        let (w_lo, w_hi) = unsafe { (_mm_loadu_si128(ptr), _mm_loadu_si128(ptr.add(1))) };
        lo = _mm256_add_epi32(lo, _mm256_madd_epi16(_mm256_cvtepi8_epi16(w_lo), xv));
        hi = _mm256_add_epi32(hi, _mm256_madd_epi16(_mm256_cvtepi8_epi16(w_hi), xv));
      }
      // 128-bit lanes: lo = [k0 k0 k1 k1 | k2 k2 k3 k3], hi = [k4 k4 k5 k5 | k6 k6 k7 k7];
      // hadd = [k0 k1 k4 k5 | k2 k3 k6 k7]; 64-bit elements (0, 2, 1, 3) restore k0..k7.
      let sum = _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_hadd_epi32(lo, hi));
      // SAFETY: `y` is a `&mut [i32; 8]`: one 32-byte unaligned store (`storeu`) is in bounds.
      unsafe { _mm256_storeu_si256(y.as_mut_ptr().cast::<__m256i>(), sum) };
    }
  }
}

#[cfg(target_arch = "aarch64")]
mod dotprod {
  use core::arch::aarch64::{
    vaddq_s32, vdotq_s32, vdupq_n_s32, vdupq_n_u32, vld1q_s8, vreinterpretq_s8_u32, vst1q_s32,
  };

  use super::{Input, Int8Matrix, Output};

  /// `sdot`: each i32 lane adds the 4-term dot product of 4 int8 pairs, i.e. one output of a
  /// block, with the input group replicated to all four lanes. Products and sums are exact in i32.
  #[target_feature(enable = "dotprod")]
  pub(super) fn gemv(m: &Int8Matrix, out: &mut Output, x: &Input) {
    // Each input group replicated to 16 bytes; 256 entries so a u8 column needs no bounds check.
    let mut xs = [vdupq_n_u32(0); 256];
    for (v, g) in xs.iter_mut().zip(x) {
      *v = vdupq_n_u32(u32::from_le_bytes(g.map(|v| v as u8)));
    }
    // SAFETY (both loads): `block` is a `&[u8; 32]`: 16-byte loads at offsets 0 and 16 are in bounds.
    let load = |block: &[u8; 32]| unsafe { (vld1q_s8(block.as_ptr().cast()), vld1q_s8(block.as_ptr().add(16).cast())) };
    let xv = |col: u8| vreinterpretq_s8_u32(xs.get(usize::from(col)).copied().unwrap_or(vdupq_n_u32(0)));
    for (y, (blocks, cols)) in out.as_chunks_mut::<8>().0.iter_mut().zip(m.groups()) {
      // Two blocks per step into separate accumulators: two independent `sdot` chains each.
      let [mut lo0, mut hi0, mut lo1, mut hi1] = [vdupq_n_s32(0); 4];
      let (pairs, last) = blocks.as_chunks::<2>();
      for ([b0, b1], [c0, c1]) in pairs.iter().zip(cols.as_chunks::<2>().0) {
        let ((w0l, w0h), (w1l, w1h)) = (load(b0), load(b1));
        let (x0, x1) = (xv(*c0), xv(*c1));
        lo0 = vdotq_s32(lo0, w0l, x0);
        hi0 = vdotq_s32(hi0, w0h, x0);
        lo1 = vdotq_s32(lo1, w1l, x1);
        hi1 = vdotq_s32(hi1, w1h, x1);
      }
      if let (Some(b), Some(&c)) = (last.first(), cols.last()) {
        let (wl, wh) = load(b);
        lo0 = vdotq_s32(lo0, wl, xv(c));
        hi0 = vdotq_s32(hi0, wh, xv(c));
      }
      let (lo, hi) = (vaddq_s32(lo0, lo1), vaddq_s32(hi0, hi1));
      let ptr = y.as_mut_ptr();
      // SAFETY: `y` is a `&mut [i32; 8]`: two 4-lane stores at offsets 0 and 4 are in bounds.
      unsafe { (vst1q_s32(ptr, lo), vst1q_s32(ptr.add(4), hi)) };
    }
  }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, reason = "tests")]
mod tests {
  use super::*;

  /// xorshift32, deterministic.
  struct Rng(u32);

  impl Rng {
    fn next(&mut self) -> u32 {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 17;
      self.0 ^= self.0 << 5;
      self.0
    }

    /// Uniform in -127..=127.
    fn q(&mut self) -> i8 {
      ((self.next() % 255) as i32 - 127) as i8
    }
  }

  fn leak(v: Vec<u8>) -> &'static [u8] {
    Box::leak(v.into_boxed_slice())
  }

  /// The selected SIMD path (AVX2 on x86_64 CPUs that have it, `dotprod` on aarch64) against
  /// the scalar reference, bit for bit, on random weights, inputs and sparse patterns.
  #[test]
  fn simd_matches_scalar() {
    let kernel = Kernel::detect();
    println!("kernel under test: {kernel:?}");
    let mut rng = Rng(0x1234_5678);
    for round in 0..50 {
      let nb_in = 4 * (1 + rng.next() as usize % MAX_GROUPS);
      let nb_out = 8 * (1 + rng.next() as usize % (MAX_OUTPUTS / 8));
      let groups = nb_in / 4;
      // Extreme rounds: all weights -128 / 127 and inputs -128 / 127 hit the largest sums.
      let fill = |rng: &mut Rng, n: usize| -> Vec<u8> {
        (0..n)
          .map(|_| match round % 5 {
            0 => 0x80,
            1 => 0x7f,
            _ => rng.next() as u8,
          })
          .collect()
      };
      let mut x = [[0i8; 4]; MAX_GROUPS];
      for v in x.iter_mut().flatten() {
        *v = match round % 5 {
          0 => i8::MIN,
          1 => 127,
          _ => rng.q(),
        };
      }
      let dense = Int8Matrix::dense(leak(fill(&mut rng, groups * nb_out / 8 * BLOCK_SIZE)), nb_in, nb_out).unwrap();
      let mut idx = Vec::new();
      for _ in 0..nb_out / 8 {
        // Random count (often 0 or full), random (possibly repeated) offsets.
        let count = match rng.next() % 4 {
          0 => 0,
          1 => groups,
          _ => rng.next() as usize % (groups + 1),
        };
        idx.push(count as u32);
        idx.extend((0..count).map(|_| 4 * (rng.next() % groups as u32)));
      }
      let blocks = idx.len() - nb_out / 8;
      let sparse = Int8Matrix::sparse(leak(fill(&mut rng, blocks * BLOCK_SIZE)), &idx, nb_in, nb_out).unwrap();
      for m in [&dense, &sparse] {
        let (mut want, mut got) = ([7i32; MAX_OUTPUTS], [7i32; MAX_OUTPUTS]);
        m.gemv_with(Kernel::SCALAR, &mut want, &x);
        m.gemv_with(kernel, &mut got, &x);
        assert_eq!(want, got, "round {round}, {nb_in} in, {nb_out} out");
        assert!(want[nb_out..].iter().all(|&v| v == 7), "outputs past nb_out touched");
      }
    }
  }

  #[test]
  fn scalar_block_layout() {
    // One dense block: output k, input c has weight k - c; x = [1, 2, 3, 4].
    let w: Vec<u8> = (0..8).flat_map(|k: i32| (0..4).map(move |c| (k - c) as i8 as u8)).collect();
    let m = Int8Matrix::dense(leak(w), 4, 8).unwrap();
    let mut x = [[0i8; 4]; MAX_GROUPS];
    x[0] = [1, 2, 3, 4];
    let mut out = [0i32; MAX_OUTPUTS];
    m.gemv_with(Kernel::SCALAR, &mut out, &x);
    for (k, &o) in (0..).zip(&out[..8]) {
      assert_eq!(o, (0..4).map(|c| (k - c) * (c + 1)).sum::<i32>());
    }
  }

  #[test]
  fn invalid_layouts_are_rejected() {
    let w = leak(vec![0; 2 * BLOCK_SIZE]);
    // Offset past the inputs, unaligned offset, too few weights, more blocks than input groups.
    assert!(Int8Matrix::sparse(w, &[1, 4], 4, 8).is_none());
    assert!(Int8Matrix::sparse(w, &[1, 2], 8, 8).is_none());
    assert!(Int8Matrix::sparse(w, &[3, 0, 0, 0], 12, 8).is_none());
    assert!(Int8Matrix::sparse(w, &[2, 0, 0], 4, 8).is_none());
    assert!(Int8Matrix::sparse(w, &[2, 0, 4], 8, 8).is_some());
    // Output count not matching the groups.
    assert!(Int8Matrix::sparse(w, &[1, 0], 4, 16).is_none());
  }
}
