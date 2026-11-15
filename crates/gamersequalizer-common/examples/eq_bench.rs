//! Measures the biquad cascade as the APO runs it against loop orders that give the CPU
//! several independent dependency chains (the alternatives are plain Rust; the cascade itself
//! takes its FMA path on x86 CPUs that have FMA, so its rows include that gain), plus the
//! limiter.
//! Prints time per 10 ms block and the largest difference from the current output.
//!
//!   cargo run --release -p gamersequalizer-common --example eq_bench

use std::hint::black_box;
use std::time::Instant;

use gamersequalizer_common::BiquadBand;
use gamersequalizer_common::biquad::{BiquadCascade, BiquadCoefficients};
use gamersequalizer_common::limiter::PeakLimiter;

const RATE: f32 = 48_000.0;
/// One 10 ms engine period.
const FRAMES: usize = 480;
const BLOCK_NS: f64 = 10_000_000.0;
const BLOCKS: usize = 2_000;
const RUNS: usize = 7;

type Kernel = Box<dyn FnMut(&mut [f32])>;

#[derive(Clone, Copy)]
struct Stage {
  b0: f64,
  b1: f64,
  b2: f64,
  a1: f64,
  a2: f64,
}

#[derive(Clone, Copy, Default)]
struct Mem {
  z1: f64,
  z2: f64,
}

fn main() {
  let cases = [
    ("mic EQ, mono", 1, 10),
    ("mic EQ full, mono", 1, 16),
    ("output EQ, stereo", 2, 20),
    ("stream EQ full, stereo", 2, 64),
    ("output EQ, 5.1 (6ch)", 6, 20),
    ("output EQ, 7.1 (8ch)", 8, 20),
  ];
  for (label, channels, stages) in cases {
    println!("\n{label}: {stages} biquads, {FRAMES} frames");
    let bands = bands(stages);
    let input = noise(FRAMES * channels);
    let reference = kernels(&bands, channels).into_iter().next().map(|(_, k)| output(k, &input)).unwrap_or_default();
    let mut base_ns = None;
    for ((name, timed), (_, checked)) in kernels(&bands, channels).into_iter().zip(kernels(&bands, channels)) {
      let ns = time(timed, &input);
      let base = *base_ns.get_or_insert(ns);
      // Not `f32::max`: it would drop a NaN and print a broken kernel as "max diff 0".
      let diffs = output(checked, &input).into_iter().zip(&reference).map(|(a, b)| (a - b).abs());
      let diff = diffs.fold(0.0f32, |m, d| if m.is_nan() || d <= m { m } else { d });
      println!(
        "  {name:<44} {ns:>8.0} ns  {:>6.3} % of a block  x{:<5.2} max diff {diff:.1e}",
        ns / BLOCK_NS * 100.0,
        base / ns
      );
    }
  }

  println!("\nlimiter (PeakLimiter + soft_limit), stereo, {FRAMES} frames");
  let quiet = noise(FRAMES * 2);
  let loud: Vec<f32> = quiet.iter().map(|s| s * 20.0).collect();
  for (name, input) in [("current, -20 dBFS (passes untouched)", quiet), ("current, +6 dBFS (limiting)", loud)] {
    let mut limiter = PeakLimiter::new(RATE);
    let ns = time(Box::new(move |buf| limiter.process(buf, 2)), &input);
    println!("  {name:<44} {ns:>8.0} ns  {:>6.3} % of a block", ns / BLOCK_NS * 100.0);
  }
}

/// Our production `BiquadCascade` first as reference, then candidate loop orders.
fn kernels(bands: &[BiquadBand], channels: usize) -> Vec<(&'static str, Kernel)> {
  let st: Vec<Stage> = bands
    .iter()
    .map(|b| {
      let c = BiquadCoefficients::from_band(b, RATE);
      Stage { b0: c.b0 as f64, b1: c.b1 as f64, b2: c.b2 as f64, a1: c.a1 as f64, a2: c.a2 as f64 }
    })
    .collect();
  let per_channel = || vec![vec![Mem::default(); st.len()]; channels];

  let mut cascades = vec![BiquadCascade::default(); channels];
  cascades.iter_mut().for_each(|c| c.set_bands(bands, RATE));
  let mut list: Vec<(&'static str, Kernel)> = vec![(
    "current: BiquadCascade::process_strided",
    Box::new(move |buf| cascades.iter_mut().enumerate().for_each(|(ch, c)| c.process_strided(buf, channels, ch))),
  )];

  let mut interleaved = BiquadCascade::with_channels(channels);
  interleaved.set_bands(bands, RATE);
  list.push((
    "current: BiquadCascade::process_interleaved",
    Box::new(move |buf| interleaved.process_interleaved(buf, channels)),
  ));

  for (name, round) in
    [("sample by sample (f32 between stages)", true), ("sample by sample (f64 between stages)", false)]
  {
    let (st, mut mem) = (st.clone(), per_channel());
    list.push((
      name,
      Box::new(move |buf| {
        mem.iter_mut().enumerate().for_each(|(ch, m)| sample_outer(&st, m, buf, channels, ch, round))
      }),
    ));
  }
  if channels == 1 {
    let (st, mut mem) = (st.clone(), vec![Mem::default(); st.len()]);
    list.push(("two stages per pass", Box::new(move |buf| stage_pairs(&st, &mut mem, buf))));
  }
  if channels == 2 {
    let (own, mut mem) = (st.clone(), vec![[Mem::default(); 2]; st.len()]);
    list.push(("stage by stage, both channels per pass", Box::new(move |buf| stereo_stage_outer(&own, &mut mem, buf))));
    for (name, round) in
      [("sample by sample, both channels (f32)", true), ("sample by sample, both channels (f64)", false)]
    {
      let (st, mut mem) = (st.clone(), vec![[Mem::default(); 2]; st.len()]);
      list.push((name, Box::new(move |buf| stereo_sample_outer(&st, &mut mem, buf, round))));
    }
  }
  list
}

/// Transposed Direct Form II, the arithmetic of a single biquad stage.
#[inline(always)]
fn step(c: &Stage, m: &mut Mem, x: f64) -> f64 {
  let y = c.b0 * x + m.z1;
  m.z1 = c.b1 * x - c.a1 * y + m.z2;
  m.z2 = c.b2 * x - c.a2 * y;
  y
}

/// Loops swapped: each sample runs through all stages. `round` keeps the f32 rounding the
/// current code does between stages (it stores every stage's output in the buffer).
#[inline(always)]
fn sample_outer(st: &[Stage], mem: &mut [Mem], buf: &mut [f32], channels: usize, channel: usize, round: bool) {
  for s in buf.iter_mut().skip(channel).step_by(channels) {
    let mut x = *s as f64;
    for (c, m) in st.iter().zip(mem.iter_mut()) {
      x = step(c, m, x);
      if round {
        x = x as f32 as f64;
      }
    }
    *s = x as f32;
  }
}

/// Mono: the second stage of a pair runs one sample behind the first, in the same pass.
fn stage_pairs(st: &[Stage], mem: &mut [Mem], buf: &mut [f32]) {
  for pair in st.chunks(2).zip(mem.chunks_mut(2)) {
    match pair {
      ([ca, cb], [ma, mb]) => {
        let mut behind: Option<(&mut f32, f32)> = None;
        for s in buf.iter_mut() {
          let first = step(ca, ma, *s as f64) as f32;
          if let Some((slot, x)) = behind.take() {
            *slot = step(cb, mb, x as f64) as f32;
          }
          behind = Some((s, first));
        }
        if let Some((slot, x)) = behind {
          *slot = step(cb, mb, x as f64) as f32;
        }
      }
      ([c], [m]) => buf.iter_mut().for_each(|s| *s = step(c, m, *s as f64) as f32),
      _ => {}
    }
  }
}

fn stereo_stage_outer(st: &[Stage], mem: &mut [[Mem; 2]], buf: &mut [f32]) {
  for (c, [ml, mr]) in st.iter().zip(mem.iter_mut()) {
    for [l, r] in buf.as_chunks_mut::<2>().0 {
      *l = step(c, ml, *l as f64) as f32;
      *r = step(c, mr, *r as f64) as f32;
    }
  }
}

#[inline(always)]
fn stereo_sample_outer(st: &[Stage], mem: &mut [[Mem; 2]], buf: &mut [f32], round: bool) {
  for [l, r] in buf.as_chunks_mut::<2>().0 {
    let (mut xl, mut xr) = (*l as f64, *r as f64);
    for (c, [ml, mr]) in st.iter().zip(mem.iter_mut()) {
      (xl, xr) = (step(c, ml, xl), step(c, mr, xr));
      if round {
        (xl, xr) = (xl as f32 as f64, xr as f32 as f64);
      }
    }
    (*l, *r) = (xl as f32, xr as f32);
  }
}

/// Best of `RUNS`: nanoseconds per block, with the block refilled before every call (the
/// copy is included, a few tens of ns).
fn time(mut kernel: Kernel, input: &[f32]) -> f64 {
  let mut buf = input.to_vec();
  let mut best = f64::MAX;
  for _ in 0..RUNS {
    let start = Instant::now();
    for _ in 0..BLOCKS {
      buf.copy_from_slice(input);
      kernel(black_box(&mut buf));
      black_box(&buf);
    }
    best = best.min(start.elapsed().as_nanos() as f64 / BLOCKS as f64);
  }
  best
}

/// 20 consecutive blocks, filter memory carried over.
fn output(mut kernel: Kernel, input: &[f32]) -> Vec<f32> {
  let mut out = Vec::new();
  for _ in 0..20 {
    let mut buf = input.to_vec();
    kernel(&mut buf);
    out.extend(buf);
  }
  out
}

/// Peaking bands from 40 Hz to 16 kHz, alternately boosting and cutting.
fn bands(count: usize) -> Vec<BiquadBand> {
  (0..count)
    .map(|i| BiquadBand {
      frequency: 40.0 * 400.0f32.powf(i as f32 / count.max(2) as f32),
      gain_db: if i % 2 == 0 { 4.0 } else { -3.0 },
      ..BiquadBand::default()
    })
    .collect()
}

/// Deterministic white noise at -20 dBFS peak.
fn noise(len: usize) -> Vec<f32> {
  let mut seed = 0x1234_5678u32;
  (0..len)
    .map(|_| {
      seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      ((seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.1
    })
    .collect()
}
