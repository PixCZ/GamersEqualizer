use super::*;
use std::f32::consts::PI;

const RATE: u32 = 48_000;
const OWNER_PID: u32 = 7;

#[test]
fn noise_mode_roundtrips_and_an_unknown_one_is_the_expander() {
  for mode in [NoiseMode::Off, NoiseMode::Expander, NoiseMode::WebRtcNs, NoiseMode::RnNoise, NoiseMode::RnNoise2] {
    assert_eq!(NoiseMode::from_u32(mode.to_u32()), mode);
  }
  assert_eq!(NoiseMode::from_u32(99), NoiseMode::Expander);
  assert!(NoiseMode::RnNoise2.is_framed() && !NoiseMode::Expander.is_framed());
}

#[test]
fn settings_are_clamped_and_garbage_becomes_the_default() {
  let s = SidetoneSettings { volume: 99.0, gate_margin_db: -5.0, gate_range_db: 500.0, ..Default::default() }.clamped();
  assert_eq!(s.volume, VOLUME_MAX);
  assert_eq!(s.gate_margin_db, GATE_MARGIN_DB_MIN);
  assert_eq!(s.gate_range_db, GATE_RANGE_DB_MAX);
  assert_eq!(SidetoneSettings { volume: f32::NAN, ..Default::default() }.clamped().volume, 1.0, "default: 100 %");
  assert_eq!(SidetoneSettings { volume: -1.0, ..Default::default() }.clamped().volume, 0.0);
}

#[test]
fn device_hash_ignores_case_and_is_never_zero() {
  assert_eq!(device_hash("{0.0.1.00000000}.{ABC}"), device_hash("{0.0.1.00000000}.{abc}"));
  assert_ne!(device_hash("{0.0.1.00000000}.{abc}"), device_hash("{0.0.0.00000000}.{abc}"));
  assert_ne!(device_hash(""), 0);
}

#[test]
fn pairs_roundtrip_ignore_unknown_fields_and_reject_garbage() {
  let mut pairs = SidetonePairs::default();
  let mut pair = SidetonePair::new("mic-A", "out-A", "Mic", "Headphones");
  pair.enabled = true;
  pair.volume = 0.5;
  pair.bands.push(BiquadBand { frequency: 3_000.0, gain_db: 4.0, ..pair.bands[0].clone() });
  pairs.upsert(pair.clone());
  pairs.upsert(SidetonePair::new("mic-A", "out-B", "Mic", "Other"));
  let back = SidetonePairs::from_json(&pairs.to_json().unwrap()).unwrap();
  assert_eq!(back, pairs);
  assert_eq!(back.find("MIC-a", "OUT-a"), Some(&pair), "ids compare without case");
  assert!(back.find("mic-B", "out-A").is_none());

  // Replacing keeps one entry per pair; removing takes only that pair.
  pair.volume = 2.0;
  pairs.upsert(pair);
  assert_eq!(pairs.pairs.len(), 2);
  assert_eq!(pairs.find("mic-A", "out-A").unwrap().volume, 2.0);
  pairs.remove("mic-A", "out-B");
  assert_eq!(pairs.pairs.len(), 1);

  // A file from another build (newer, or the first sidetone build with `gain_db` and the
  // cuts): unknown fields ignored, missing ones defaulted, unknown mode kept.
  let other =
    r#"{"pairs":[{"mic_id":"m","output_id":"o","noise_mode":77,"gain_db":24.0,"low_cut_hz":90.0}],"extra":1}"#;
  let parsed = SidetonePairs::from_json(other).unwrap();
  let p = parsed.find("m", "o").unwrap();
  assert_eq!((p.noise_mode, p.volume, p.enabled), (77, 1.0, false));
  assert_eq!(p.bands, default_bands());
  assert_eq!(p.settings().noise_mode, NoiseMode::Expander);
  assert!(SidetonePairs::from_json("{ not json").is_err());
}

#[test]
fn ring_needs_its_owner_and_reads_garbage_as_silence() {
  let ring = SidetoneRing::default();
  let owner = ring.claim_writer(OWNER_PID, 1).unwrap();
  assert!(ring.claim_writer(OWNER_PID, 2).is_none(), "one writer per process");
  let other = ring.claim_writer(OWNER_PID + 1, 1).unwrap();
  assert!(!ring.write(owner, &[0.5; 4], RATE, 0), "the first owner was replaced (another process = stale)");
  assert_eq!(ring.write_index(), 0);
  assert!(ring.write(other, &[0.25, f32::NAN, 100.0, -0.25], RATE, 10_000_000));
  assert_eq!(ring.write_index(), 4);
  assert_eq!([ring.sample(0), ring.sample(1), ring.sample(2), ring.sample(3)], [0.25, 0.0, 0.0, -0.25]);
  // 48 samples behind the head = 1 ms earlier.
  assert_eq!(ring.capture_qpc_hns(48), Some(10_000_000 - 10_000));
  ring.release_writer(other);
  assert!(ring.claim_writer(OWNER_PID, 3).is_some());
}

#[test]
fn ring_owner_that_stopped_writing_is_replaced() {
  let ring = SidetoneRing::default();
  let first = ring.claim_writer(OWNER_PID, 1).unwrap();
  // While the owner writes, another instance of the same process never gets the ring.
  for _ in 0..3 * STALE_CLAIMS {
    assert!(ring.write(first, &[0.1; 480], RATE, 0));
    assert!(ring.claim_writer(OWNER_PID, 2).is_none());
  }
  // Its stream stopped without a release: taken over after STALE_CLAIMS tries.
  let tries = (1..=2 * STALE_CLAIMS).find(|_| ring.claim_writer(OWNER_PID, 2).is_some()).unwrap();
  assert_eq!(tries, STALE_CLAIMS);
  assert!(!ring.write(first, &[0.1; 480], RATE, 0), "the old owner is told it lost the ring");
}

#[test]
fn reader_plays_a_microphone_at_384_khz() {
  // One output block needs 3840 ring samples: the queue limits must leave room to start.
  let run = simulate(&Sim { seconds: 3.0, writer_rate: 384_000, writer_block: 3_840, ..Sim::default() });
  assert_eq!(run.underruns, 0);
  assert!(rms(&run.out[2 * RATE as usize..]) > 0.3);
}

#[test]
fn reader_plays_the_writer_with_the_same_clock_exactly() {
  let run = simulate(&Sim { seconds: 3.0, ..Sim::default() });
  assert_eq!((run.underruns, run.resyncs), (0, 0));
  // Same rate and no drift: the interpolator sits on whole samples, the output is a copy.
  let tail = &run.out[RATE as usize..];
  assert!(max_step(tail) < TONE_MAX_STEP * 1.01, "no discontinuity");
  let rms = rms(tail);
  assert!((rms - TONE_AMPLITUDE / 2f32.sqrt()).abs() < 0.005, "level kept: {rms}");
}

#[test]
fn reader_follows_a_drifting_clock_without_gaps() {
  for ppm in [-200.0, 200.0] {
    // Mic blocks of 10 ms, output blocks of 2.67 ms, calls up to 0.5 ms late.
    let run =
      simulate(&Sim { seconds: 60.0, writer_ppm: ppm, reader_block: 128, jitter_seconds: 0.0005, ..Sim::default() });
    assert_eq!((run.underruns, run.resyncs), (0, 0), "{ppm} ppm");
    assert!(max_step(&run.out[RATE as usize..]) < TONE_MAX_STEP * 1.2, "{ppm} ppm: no discontinuity");
    // Queue at each call: at most one writer block + one reader block + margin + a little.
    let limit = 480 + 128 + (MARGIN_SECONDS * RATE as f32) as u32 + 96;
    assert!(run.max_queued_late <= limit, "{ppm} ppm: queued {} > {limit}", run.max_queued_late);
  }
}

#[test]
fn reader_moves_up_after_the_start() {
  // Both APOs at 10 ms: after the first window the queue is one block + margin, not the
  // 10 ms extra it started with.
  let run = simulate(&Sim { seconds: 3.0, reader_phase_seconds: 0.004, ..Sim::default() });
  assert_eq!(run.underruns, 0);
  // The writer's block lands 6 ms before each read; the rest is what the reader keeps.
  let limit = 480 + (MARGIN_SECONDS * RATE as f32) as u32 + 96;
  assert!(run.max_queued_late <= limit, "queued {} > {limit}", run.max_queued_late);
}

#[test]
fn reader_converts_the_sample_rate() {
  let run = simulate(&Sim { seconds: 3.0, writer_rate: 44_100, writer_block: 441, ..Sim::default() });
  assert_eq!(run.underruns, 0);
  let tail = &run.out[2 * RATE as usize..];
  let crossings = tail.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
  assert!((crossings as i32 - TONE_HZ as i32).abs() <= 1, "pitch kept: {crossings} Hz");
}

#[test]
fn reader_goes_silent_when_the_writer_stops_and_comes_back() {
  let run = simulate(&Sim { seconds: 4.0, writer_pause: Some((1.5, 2.5)), ..Sim::default() });
  assert_eq!(run.underruns, 1);
  let at = |seconds: f32| (seconds * RATE as f32) as usize;
  assert!(rms(&run.out[at(1.7)..at(2.4)]) == 0.0, "silent while the writer is gone");
  assert!(rms(&run.out[at(2.7)..at(3.9)]) > 0.2, "back after it returned");
}

#[test]
fn reader_drops_what_piled_up_while_it_was_not_called() {
  let ring = SidetoneRing::default();
  let owner = ring.claim_writer(OWNER_PID, 1).unwrap();
  let mut reader = SidetoneReader::default();
  let mut block = [0.0f32; 480];
  let mut step = |reader: &mut SidetoneReader, read: bool| {
    ring.write(owner, &[0.25; 480], RATE, 0);
    read.then(|| reader.read(&ring, &mut block, RATE as f32))
  };
  let playing = (0..50).filter_map(|_| step(&mut reader, true)).last().unwrap();
  assert!(playing.playing);
  // 200 ms without a read (the output engine stood still), then reading again.
  (0..20).for_each(|_| assert!(step(&mut reader, false).is_none()));
  assert!(!step(&mut reader, true).unwrap().playing, "the old queue is not played");
  let again = (0..50).filter_map(|_| step(&mut reader, true)).last().unwrap();
  assert!(again.playing && again.queued < 1_500, "back at a short queue: {}", again.queued);
  assert_eq!(reader.underruns(), 0);

  // Paused on purpose: the same fresh start.
  reader.pause();
  assert!(!step(&mut reader, true).unwrap().playing);
}

#[test]
fn reader_does_not_count_a_writer_that_stopped_on_purpose() {
  let run = simulate(&Sim { seconds: 4.0, writer_pause: Some((1.5, 2.5)), release_in_pause: true, ..Sim::default() });
  assert_eq!(run.underruns, 0, "released ring: no gap counted, the margin stays");
  let at = |seconds: f32| (seconds * RATE as f32) as usize;
  assert!(rms(&run.out[at(1.7)..at(2.4)]) == 0.0);
  assert!(rms(&run.out[at(2.7)..at(3.9)]) > 0.2, "back after the writer returned");
}

#[test]
fn expander_turns_steady_noise_down_by_its_depth() {
  let mut expander = BandExpander::new(RATE as f32);
  expander.set(10.0, 18.0);
  let input = noise(6 * RATE as usize, 0.001, 3);
  let mut out = input.clone();
  process_in_blocks(&mut out, |b| expander.process(b));
  let cut = db(rms(&out[4 * RATE as usize..]) / rms(&input[4 * RATE as usize..]));
  assert!((-19.0..=-16.0).contains(&cut), "noise changed by {cut:.1} dB");
}

#[test]
fn expander_closes_on_noise_at_once_and_stays_closed_after_a_reset() {
  let mut expander = BandExpander::new(RATE as f32);
  let mut block = noise(2 * RATE as usize, 0.001, 4);
  let input = block.clone();
  process_in_blocks(&mut block, |b| expander.process(b));
  // Not 3 s later, when the first floor window has gone: from the first half second.
  let half = RATE as usize / 2;
  let cut = db(rms(&block[half..]) / rms(&input[half..]));
  assert!(cut < -15.0, "noise from 0.5 s: {cut:.1} dB");
  // After a pause (mute, pair switch) the floors are still known.
  expander.reset();
  let mut block = input.clone();
  process_in_blocks(&mut block, |b| expander.process(b));
  let cut = db(rms(&block[..half]) / rms(&input[..half]));
  assert!(cut < -15.0, "noise right after a reset: {cut:.1} dB");
}

#[test]
fn expander_opens_for_a_voice_within_two_milliseconds() {
  let mut expander = BandExpander::new(RATE as f32);
  let onset = 5 * RATE as usize;
  let mut input = noise(6 * RATE as usize, 0.001, 5);
  for (i, s) in input.iter_mut().enumerate().skip(onset) {
    *s += voice(i - onset, 0.03);
  }
  let mut out = input.clone();
  let (before, after) = out.split_at_mut(onset);
  process_in_blocks(before, |b| expander.process(b));
  assert!(expander.gain_db() < -17.0, "closed before the voice: {:.1} dB", expander.gain_db());
  process_in_blocks(after, |b| expander.process(b));
  let ms = RATE as usize / 1000;
  let early = db(rms(&out[onset + 2 * ms..onset + 20 * ms]) / rms(&input[onset + 2 * ms..onset + 20 * ms]));
  let steady = db(rms(&out[onset + 100 * ms..]) / rms(&input[onset + 100 * ms..]));
  assert!(early > -1.5, "2-20 ms after the onset: {early:.1} dB");
  assert!(steady > -0.5, "steady voice: {steady:.1} dB");
}

#[test]
fn expander_takes_hiss_from_under_a_low_tone() {
  // Hiss above 6 kHz at -60 dBFS under a 200 Hz tone at -20 dBFS.
  let len = 6 * RATE as usize;
  let mut hiss = noise(len, 0.004, 9);
  high_pass(&mut hiss, 6_000.0);
  let tone: Vec<f32> = (0..len).map(|i| 0.1 * (2.0 * PI * 200.0 * i as f32 / RATE as f32).sin()).collect();
  let input: Vec<f32> = hiss.iter().zip(&tone).map(|(h, t)| h + t).collect();
  let tail = 4 * RATE as usize;
  let mut expander = BandExpander::new(RATE as f32);
  let mut out = input.clone();
  process_in_blocks(&mut out, |b| expander.process(b));
  let tone_change = db(rms(&out[tail..]) / rms(&input[tail..]));
  // The bands overlap gently: a closed neighbour takes a little of the tone with it.
  assert!(tone_change.abs() < 1.0, "tone changed by {tone_change:.2} dB");
  let (mut a, mut b) = (out, input.clone());
  high_pass(&mut a, 4_000.0);
  high_pass(&mut b, 4_000.0);
  let hiss = db(rms(&a[tail..]) / rms(&b[tail..]));
  assert!(hiss < -12.0, "hiss under the tone changed by {hiss:.1} dB");
}

#[test]
fn expander_with_no_depth_returns_the_input() {
  let mut expander = BandExpander::new(RATE as f32);
  expander.set(10.0, 0.0);
  let input: Vec<f32> = noise(RATE as usize, 0.1, 11).iter().enumerate().map(|(i, n)| n + voice(i, 0.1)).collect();
  let mut out = input.clone();
  process_in_blocks(&mut out, |b| expander.process(b));
  // Gains start at 0 and open within a few ms: compare after that.
  let skip = RATE as usize / 10;
  let diff: Vec<f32> = out.iter().zip(&input).skip(skip).map(|(a, b)| a - b).collect();
  let error = db(rms(&diff) / rms(&input[skip..]));
  assert!(error < -90.0, "bands do not add up to the input: {error:.0} dB");
}

#[test]
fn chain_survives_garbage_and_ramps_its_level() {
  let mut chain = SidetoneChain::new(RATE as f32);
  let on = SidetoneSettings { enabled: true, noise_mode: NoiseMode::Off, ..Default::default() };
  chain.configure(&on, 0.0, false);
  assert!(!chain.is_silent());
  let mut block = vec![0.5f32; 4_800];
  block[10] = f32::NAN;
  block[11] = f32::INFINITY;
  chain.process(&mut block);
  assert!(block.iter().all(|s| s.is_finite()));
  assert!(max_step(&block[100..]) < 0.01, "the level ramps, no jump");
  assert!((block[4_799] - 0.5).abs() < 1e-3, "100 % reached: {}", block[4_799]);

  // The leveler lifts a quiet voice by 12 dB: so does the sidetone, times the volume (50 %).
  chain.configure(&SidetoneSettings { volume: 0.5, ..on }, 12.0, false);
  let mut block = vec![0.1f32; 4_800];
  chain.process(&mut block);
  assert!((block[4_799] - 0.1 * 0.5 * 10f32.powf(12.0 / 20.0)).abs() < 1e-3, "{}", block[4_799]);

  // Garbage instead of the leveler's gain: the limit, or no gain.
  for (garbage, level) in [(900.0, 0.1 * 10f32.powf(LEVELER_GAIN_LIMIT_DB / 20.0)), (f32::NAN, 0.1)] {
    chain.configure(&on, garbage, false);
    let mut block = vec![0.1f32; 4_800];
    chain.process(&mut block);
    assert!((block[4_799] - level).abs() < 1e-3, "{garbage}: {}", block[4_799]);
  }

  // Muted: down to exactly zero, then nothing to write.
  chain.configure(&on, 0.0, true);
  let mut block = vec![0.5f32; 4_800];
  chain.process(&mut block);
  assert!(max_step(&block) < 0.01);
  assert_eq!(block[4_799], 0.0);
  assert!(chain.is_silent());
}

#[test]
fn chain_default_eq_removes_rumble() {
  let mut chain = SidetoneChain::new(RATE as f32);
  chain.configure(&SidetoneSettings { enabled: true, noise_mode: NoiseMode::Off, ..Default::default() }, 0.0, false);
  chain.set_eq(&default_bands());
  let tone =
    |hz: f32| (0..RATE as usize).map(|i| 0.5 * (2.0 * PI * hz * i as f32 / RATE as f32).sin()).collect::<Vec<f32>>();
  let mut gain = |hz: f32| {
    let input = tone(hz);
    let mut out = input.clone();
    chain.process(&mut out);
    db(rms(&out[RATE as usize / 2..]) / rms(&input[RATE as usize / 2..]))
  };
  assert!(gain(50.0) < -20.0);
  assert!(gain(1_000.0).abs() < 0.5);
}

// --- helpers ---

const TONE_HZ: f32 = 440.0;
const TONE_AMPLITUDE: f32 = 0.5;
/// Largest sample-to-sample step of the test tone at 48 kHz.
const TONE_MAX_STEP: f32 = 2.0 * PI * TONE_HZ / RATE as f32 * TONE_AMPLITUDE;

/// A writer and a reader on their own clocks, in simulated time.
struct Sim {
  seconds: f64,
  writer_rate: u32,
  writer_block: usize,
  /// The writer's clock runs this much fast.
  writer_ppm: f64,
  reader_block: usize,
  /// First reader call this long after the first writer block.
  reader_phase_seconds: f64,
  /// Reader calls come up to this late.
  jitter_seconds: f64,
  /// The writer delivers nothing between these two times.
  writer_pause: Option<(f64, f64)>,
  /// The writer gives the ring up at the start of the pause and takes it again after it.
  release_in_pause: bool,
}

impl Default for Sim {
  fn default() -> Self {
    Self {
      seconds: 1.0,
      writer_rate: RATE,
      writer_block: 480,
      writer_ppm: 0.0,
      reader_block: 480,
      reader_phase_seconds: 0.0013,
      jitter_seconds: 0.0,
      writer_pause: None,
      release_in_pause: false,
    }
  }
}

struct Run {
  out: Vec<f32>,
  underruns: u32,
  resyncs: u32,
  /// Largest queue seen by a call after the first second.
  max_queued_late: u32,
}

fn simulate(sim: &Sim) -> Run {
  let ring = SidetoneRing::default();
  let mut owner = ring.claim_writer(OWNER_PID, 1);
  let mut reader = SidetoneReader::default();
  let writer_period = sim.writer_block as f64 / (f64::from(sim.writer_rate) * (1.0 + sim.writer_ppm * 1e-6));
  let reader_period = sim.reader_block as f64 / f64::from(RATE);
  let (mut next_write, mut next_read) = (0.0, sim.reader_phase_seconds);
  let (mut written, mut reads) = (0u64, 0u64);
  let mut seed = 1u32;
  let mut out = Vec::new();
  let mut max_queued_late = 0;
  let mut block = vec![0.0f32; sim.reader_block];
  while next_read < sim.seconds {
    if next_write <= next_read {
      let paused = sim.writer_pause.is_some_and(|(from, to)| next_write >= from && next_write < to);
      if paused
        && sim.release_in_pause
        && let Some(owner) = owner.take()
      {
        ring.release_writer(owner);
      }
      if !paused {
        let owner = *owner.get_or_insert_with(|| ring.claim_writer(OWNER_PID, 1).unwrap());
        let samples: Vec<f32> = (0..sim.writer_block)
          .map(|i| {
            TONE_AMPLITUDE
              * (2.0 * PI * TONE_HZ * ((written + i as u64) % u64::from(sim.writer_rate)) as f32
                / sim.writer_rate as f32)
                .sin()
          })
          .collect();
        ring.write(owner, &samples, sim.writer_rate, 0);
      }
      // The tone keeps its phase through a pause (the samples are lost, not delayed).
      written += sim.writer_block as u64;
      next_write += writer_period;
    } else {
      let info = reader.read(&ring, &mut block, RATE as f32);
      if next_read > 1.0 && info.playing {
        max_queued_late = max_queued_late.max(info.queued + sim.reader_block as u32);
      }
      out.extend_from_slice(&block);
      reads += 1;
      seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      let late = f64::from(seed >> 8) / f64::from(1u32 << 24) * sim.jitter_seconds;
      next_read = sim.reader_phase_seconds + reads as f64 * reader_period + late;
    }
  }
  Run { out, underruns: reader.underruns(), resyncs: reader.resyncs(), max_queued_late }
}

fn process_in_blocks(samples: &mut [f32], mut f: impl FnMut(&mut [f32])) {
  samples.chunks_mut(480).for_each(&mut f);
}

/// White noise, uniform in +-`amplitude`.
fn noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
  let mut state = seed;
  (0..len)
    .map(|_| {
      state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      amplitude * ((state >> 8) as f32 / 8_388_608.0 - 1.0)
    })
    .collect()
}

/// Harmonics of 150 Hz up to 6 kHz, falling 6 dB per octave.
fn voice(i: usize, amplitude: f32) -> f32 {
  let t = i as f32 / RATE as f32;
  (1..=40).map(|h| amplitude / h as f32 * (2.0 * PI * 150.0 * h as f32 * t).sin()).sum()
}

fn high_pass(samples: &mut [f32], hz: f32) {
  let mut filter = BiquadCascade::default();
  let band = BiquadBand {
    enabled: true,
    filter_type: FilterType::HighPass,
    frequency: hz,
    gain_db: 0.0,
    q: 0.707,
    slope_db: 48,
  };
  filter.set_bands(&[band], RATE as f32);
  filter.process(samples);
}

fn rms(samples: &[f32]) -> f32 {
  (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt() as f32
}

fn db(ratio: f32) -> f32 {
  20.0 * ratio.max(1e-9).log10()
}

fn max_step(samples: &[f32]) -> f32 {
  samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}
