//! Per-stream table in the APO mapping. Every stream-effect (SFX) APO instance (Windows
//! creates one per app audio stream on a render endpoint, before the mix) claims a slot and
//! publishes its format, processing mode and a level envelope.
//!
//! The APO runs inside audiodg and cannot see which app owns its stream. The daemon (and the
//! `stream_probe` example) pairs slots with audio sessions instead, see `hold_match`.
//! Measured (Milestone 19): a session's peak meter is the max of the very same samples the
//! SFX sees over the last ~100 ms, bit for bit, so a stream's envelope run through that
//! max-hold reproduces its session's meter exactly; other streams don't.
//!
//! All fields are atomics in zero-initialized shared memory: all-zero = free slot.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// Simultaneous streams tracked. Shortcut: an instance that finds no free slot runs
/// unregistered (pass-through, never paired); raise if `stream_probe` ever shows a full table.
pub const STREAM_SLOTS: usize = 64;
/// Envelope resolution: one peak per 10 ms of QPC time (100 ns units).
pub const ENVELOPE_BLOCK_HNS: u64 = 100_000;
/// 256 blocks = 2.56 s of history.
pub const ENVELOPE_LEN: usize = 256;

/// Max-hold windows tried (blocks; 10 = 100 ms measured) and APO-vs-meter offsets (+-50 ms).
pub const HOLD_MAX_WINDOW: usize = 15;
pub const HOLD_MAX_LAG: usize = 5;
/// Envelope blocks `hold_match` needs before and after the meter series.
pub const HOLD_PAD_BEFORE: usize = HOLD_MAX_WINDOW + HOLD_MAX_LAG;
pub const HOLD_PAD_AFTER: usize = HOLD_MAX_LAG;
/// Fewer points with audio on both sides than this: no verdict.
const HOLD_MIN_POINTS: usize = 20;
/// Relative error allowed for a point to count as a match (the measured match is exact;
/// this absorbs a session volume applied as a float multiply).
const HOLD_TOLERANCE: f32 = 1e-4;
/// Below -120 dBFS: dither/denormal noise, treated as silence.
pub const SILENCE: f32 = 1e-6;
/// A pairing needs this match share and this lead over the runner-up session. Below 1 on
/// purpose: a session with several streams matches each only where it was the loudest.
/// Measured: the right session 93-100 %, others <= 6 % (Milestone 19).
pub const PAIR_MIN_SCORE: f32 = 0.5;
pub const PAIR_MIN_MARGIN: f32 = 0.3;
/// Chat and media have their own stream EQ.
pub const STREAM_EQ_BUSES: usize = 2;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// One stream (one SFX instance). Written by that instance only, read by anyone.
#[repr(C)]
pub struct StreamSlot {
  /// 0 = free, else `(audiodg pid << 32) | instance number`. A slot owned by another pid
  /// is stale (audiodg restarted while the daemon kept the mapping open) and may be reclaimed.
  pub owner: AtomicU64,
  pub created_hns: AtomicU64,
  /// QPC time of the last processed (non-silent) buffer.
  pub last_audio_hns: AtomicU64,
  pub frames: AtomicU64,
  pub sample_rate: AtomicU32,
  pub channels: AtomicU32,
  /// `AudioProcessingMode` GUID (as u128, high and low half); 0 = not reported.
  pub mode_hi: AtomicU64,
  pub mode_lo: AtomicU64,
  /// Peak |sample| per envelope block (f32 bits), at index `block % ENVELOPE_LEN`.
  pub env_peak: [AtomicU32; ENVELOPE_LEN],
  /// Low 32 bits of the block number each `env_peak` entry belongs to; an entry with another
  /// tag is old (that block was silent).
  pub env_block: [AtomicU32; ENVELOPE_LEN],
  /// Channel this stream belongs to (`StreamBus`), written by the daemon after pairing.
  pub bus: AtomicU32,
}

/// Channel of one app stream. Game (and anything unpaired) needs no stream EQ: the output EQ
/// after the mix already carries headphone + game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamBus {
  None,
  Chat,
  Media,
  Unknown(u32),
}

#[repr(C)]
pub struct StreamTable {
  pub next_instance: AtomicU32,
  pub slots: [StreamSlot; STREAM_SLOTS],
}

/// Best way a stream's envelope explains a session meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldMatch {
  /// Share of points (audio on both sides) where meter == gain * max-hold(envelope).
  pub score: f32,
  /// Max-hold length and offset (blocks) that matched best.
  pub window: usize,
  pub lag: isize,
  /// meter / stream level; 1.0 measured with session volume 100 %.
  pub gain: f32,
  pub points: usize,
}

/// What a slot was claimed with.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
  pub pid: u32,
  pub now_hns: u64,
  pub sample_rate: u32,
  pub channels: u32,
  pub mode: u128,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl StreamBus {
  /// Buses with their own stream EQ, in `ShmHeader::stream_eq` order.
  pub const WITH_EQ: [StreamBus; STREAM_EQ_BUSES] = [StreamBus::Chat, StreamBus::Media];

  pub fn to_u32(self) -> u32 {
    match self {
      StreamBus::None => 0,
      StreamBus::Chat => 1,
      StreamBus::Media => 2,
      StreamBus::Unknown(v) => v,
    }
  }

  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => StreamBus::None,
      1 => StreamBus::Chat,
      2 => StreamBus::Media,
      other => StreamBus::Unknown(other),
    }
  }

  /// Index into `ShmHeader::stream_eq`; None = no stream EQ (pass-through).
  pub fn eq_index(self) -> Option<usize> {
    Self::WITH_EQ.iter().position(|b| *b == self)
  }
}

impl StreamTable {
  /// Claims a free (or stale) slot: returns its index and owner value, None when full.
  pub fn claim(&self, info: &StreamInfo) -> Option<(usize, u64)> {
    let number = self.next_instance.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1);
    let owner = (u64::from(info.pid) << 32) | u64::from(number);
    for (index, slot) in self.slots.iter().enumerate() {
      let current = slot.owner.load(Ordering::Acquire);
      let free = current == 0 || (current >> 32) as u32 != info.pid;
      if free && slot.owner.compare_exchange(current, owner, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        slot.reset(info);
        return Some((index, owner));
      }
    }
    None
  }

  /// Frees the slot if `owner` still holds it.
  pub fn release(&self, index: usize, owner: u64) {
    if let Some(slot) = self.slots.get(index) {
      let _ = slot.owner.compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
  }
}

impl StreamSlot {
  /// Called by the owning instance for each non-silent buffer.
  pub fn record(&self, now_hns: u64, peak: f32, frames: u32) {
    let block = now_hns / ENVELOPE_BLOCK_HNS;
    let index = (block % ENVELOPE_LEN as u64) as usize;
    let tag = block as u32;
    if let (Some(peak_cell), Some(tag_cell)) = (self.env_peak.get(index), self.env_block.get(index)) {
      // Single writer per slot: load + store is enough. A reader may briefly see a new tag
      // with the old peak; one wrong 10 ms point does not move a 2 s correlation.
      if tag_cell.load(Ordering::Relaxed) == tag {
        let max = f32::from_bits(peak_cell.load(Ordering::Relaxed)).max(peak);
        peak_cell.store(max.to_bits(), Ordering::Relaxed);
      } else {
        peak_cell.store(peak.to_bits(), Ordering::Relaxed);
        tag_cell.store(tag, Ordering::Release);
      }
    }
    self.last_audio_hns.store(now_hns, Ordering::Relaxed);
    self.frames.fetch_add(u64::from(frames), Ordering::Relaxed);
  }

  /// Peaks of blocks `first_block..first_block + out.len()`; blocks without audio (or older
  /// than the history) read as 0.
  pub fn envelope(&self, first_block: u64, out: &mut [f32]) {
    for (offset, value) in out.iter_mut().enumerate() {
      let block = first_block + offset as u64;
      let index = (block % ENVELOPE_LEN as u64) as usize;
      *value = match (self.env_block.get(index), self.env_peak.get(index)) {
        (Some(tag), Some(peak)) if tag.load(Ordering::Acquire) == block as u32 => {
          f32::from_bits(peak.load(Ordering::Relaxed))
        }
        _ => 0.0,
      };
    }
  }

  pub fn mode(&self) -> u128 {
    (u128::from(self.mode_hi.load(Ordering::Relaxed)) << 64) | u128::from(self.mode_lo.load(Ordering::Relaxed))
  }

  fn reset(&self, info: &StreamInfo) {
    self.created_hns.store(info.now_hns, Ordering::Relaxed);
    self.last_audio_hns.store(0, Ordering::Relaxed);
    self.frames.store(0, Ordering::Relaxed);
    self.sample_rate.store(info.sample_rate, Ordering::Relaxed);
    self.channels.store(info.channels, Ordering::Relaxed);
    self.mode_hi.store((info.mode >> 64) as u64, Ordering::Relaxed);
    self.mode_lo.store(info.mode as u64, Ordering::Relaxed);
    self.bus.store(StreamBus::None.to_u32(), Ordering::Relaxed);
    // The previous owner's tags would otherwise show up as this stream's audio for 2.56 s.
    // Tag 0 is a real block number only once every ~497 days of uptime.
    self.env_block.iter().for_each(|t| t.store(0, Ordering::Relaxed));
  }
}

/// The session a stream belongs to, from its matches against every session: the best one
/// if it passes `PAIR_MIN_SCORE` and leads the runner-up by `PAIR_MIN_MARGIN`.
/// Returns (key, its match, runner-up score).
pub fn pick_session<K: Copy>(matches: &[(K, HoldMatch)]) -> Option<(K, HoldMatch, f32)> {
  let best = matches.iter().max_by(|a, b| a.1.score.total_cmp(&b.1.score))?;
  let runner_up = matches.iter().filter(|m| !std::ptr::eq(*m, best)).map(|m| m.1.score).fold(0.0f32, f32::max);
  (best.1.score >= PAIR_MIN_SCORE && best.1.score - runner_up >= PAIR_MIN_MARGIN).then_some((best.0, best.1, runner_up))
}

/// How well `envelope` (one stream) explains `meter` (one session's peaks, same blocks):
/// tries every max-hold window and offset, takes the median meter/stream ratio as gain,
/// and counts the points that match exactly up to that gain.
///
/// `envelope` must start `HOLD_PAD_BEFORE` blocks before `meter[0]` and end `HOLD_PAD_AFTER`
/// blocks after its last block; otherwise None. None also when fewer than 20 points have
/// audio on both sides. A session with several streams reports their combined max, so
/// each of its streams matches only where it was the loudest (score below 1).
pub fn hold_match(envelope: &[f32], meter: &[f32]) -> Option<HoldMatch> {
  if envelope.len() != meter.len() + HOLD_PAD_BEFORE + HOLD_PAD_AFTER {
    return None;
  }
  let mut best: Option<HoldMatch> = None;
  let mut pairs: Vec<(f32, f32)> = Vec::with_capacity(meter.len());
  let mut ratios: Vec<f32> = Vec::with_capacity(meter.len());
  for window in 1..=HOLD_MAX_WINDOW {
    for lag in -(HOLD_MAX_LAG as isize)..=HOLD_MAX_LAG as isize {
      pairs.clear();
      for (t, &m) in meter.iter().enumerate() {
        // Last envelope block the meter value can contain; never negative (pad >= lag).
        let Some(end) = (HOLD_PAD_BEFORE + t).checked_add_signed(lag) else { continue };
        let held = end
          .checked_sub(window - 1)
          .and_then(|start| envelope.get(start..=end))
          .map_or(0.0, |s| s.iter().fold(0.0f32, |acc, v| acc.max(*v)));
        if m > SILENCE && held > SILENCE {
          pairs.push((m, held));
        }
      }
      if pairs.len() < HOLD_MIN_POINTS {
        continue;
      }
      ratios.clear();
      ratios.extend(pairs.iter().map(|(m, a)| m / a));
      ratios.sort_by(f32::total_cmp);
      let Some(&gain) = ratios.get(ratios.len() / 2) else { continue };
      let matched = pairs.iter().filter(|(m, a)| (m - gain * a).abs() <= HOLD_TOLERANCE * m).count();
      let score = matched as f32 / pairs.len() as f32;
      if best.is_none_or(|b| score > b.score) {
        best = Some(HoldMatch { score, window, lag, gain, points: pairs.len() });
      }
    }
  }
  best
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  fn table() -> Box<StreamTable> {
    let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
    // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping.
    unsafe {
      std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
      mem.assume_init()
    }
  }

  fn info(pid: u32) -> StreamInfo {
    StreamInfo { pid, now_hns: 5, sample_rate: 48_000, channels: 2, mode: 0x1234_u128 << 64 | 7 }
  }

  #[test]
  fn claim_release_and_stale_owner_reclaim() {
    let t = table();
    let (a, owner_a) = t.claim(&info(100)).unwrap();
    let (b, _) = t.claim(&info(100)).unwrap();
    assert_ne!(a, b);
    assert_eq!(t.slots[a].mode(), 0x1234_u128 << 64 | 7);
    t.release(a, owner_a);
    assert_eq!(t.slots[a].owner.load(Ordering::Relaxed), 0);
    // A new audiodg (other pid) takes over the old one's slots once the free ones are used up.
    for _ in 0..STREAM_SLOTS - 1 {
      t.claim(&info(100)).unwrap();
    }
    assert!(t.claim(&info(100)).is_none());
    assert!(t.claim(&info(200)).is_some());
  }

  #[test]
  fn envelope_keeps_block_peaks_and_reads_gaps_as_silence() {
    let t = table();
    let (i, _) = t.claim(&info(1)).unwrap();
    let slot = &t.slots[i];
    let base = 1_000 * ENVELOPE_BLOCK_HNS;
    slot.record(base, 0.2, 480);
    slot.record(base + 50_000, 0.5, 480); // same block: max
    slot.record(base + 2 * ENVELOPE_BLOCK_HNS, 0.1, 480);
    let mut out = [9.0; 4];
    slot.envelope(1_000, &mut out);
    assert_eq!(out, [0.5, 0.0, 0.1, 0.0]);
    assert_eq!(slot.frames.load(Ordering::Relaxed), 1440);
  }

  #[test]
  fn reclaimed_slot_does_not_inherit_the_old_envelope() {
    let t = table();
    let (i, owner) = t.claim(&info(1)).unwrap();
    t.slots[i].record(1_000 * ENVELOPE_BLOCK_HNS, 0.7, 480);
    t.release(i, owner);
    let (j, _) = t.claim(&info(1)).unwrap();
    assert_eq!(i, j);
    let mut out = [9.0; 1];
    t.slots[j].envelope(1_000, &mut out);
    assert_eq!(out, [0.0]);
  }

  #[test]
  fn hold_match_recovers_window_lag_and_gain_and_rejects_other_streams() {
    const N: usize = 200;
    let len = N + HOLD_PAD_BEFORE + HOLD_PAD_AFTER;
    let stream = noise(1, len);
    let other = noise(2, len);
    // What Windows did in the measurement: 100 ms max-hold, one block behind; here at -6 dB.
    let meter: Vec<f32> = (0..N)
      .map(|t| {
        let end = HOLD_PAD_BEFORE + t - 1;
        stream[end - 9..=end].iter().fold(0.0f32, |m, v| m.max(*v)) * 0.5
      })
      .collect();

    let hit = hold_match(&stream, &meter).unwrap();
    assert_eq!((hit.score, hit.window, hit.lag), (1.0, 10, -1));
    assert!((hit.gain - 0.5).abs() < 1e-6);
    let miss = hold_match(&other, &meter).unwrap();
    assert!(miss.score < 0.2, "{miss:?}");

    assert!(hold_match(&stream[1..], &meter).is_none(), "wrong padding");
    assert!(hold_match(&vec![0.0; len], &meter).is_none(), "silent stream");
  }

  /// Measured round (Brave playing music, The Division 2 silent in the background): the
  /// browser's stream must pair with brave.exe and not with our loopback (= the whole mix).
  #[test]
  fn hold_match_pairs_the_recorded_browser_stream() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/stream_probe/brave_music_round.csv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let names: Vec<&str> = lines.next().unwrap().split(',').collect();
    let rows: Vec<Vec<f32>> = lines.map(|l| l.split(',').map(|v| v.parse().unwrap()).collect()).collect();
    let column = |name: &str| -> Vec<f32> {
      let i = names.iter().position(|n| n.starts_with(name)).unwrap();
      rows.iter().map(|r| r[i]).collect()
    };
    // The recording has no history before the round: pad with silence.
    let mut envelope = vec![0.0; HOLD_PAD_BEFORE];
    envelope.extend(column("slot0"));
    envelope.extend([0.0; HOLD_PAD_AFTER]);

    let brave = hold_match(&envelope, &column("brave.exe")).unwrap();
    assert!(brave.score > 0.9, "{brave:?}");
    assert_eq!((brave.window, brave.lag), (10, -1));
    assert!((brave.gain - 1.0).abs() < 1e-4);
    let mix = hold_match(&envelope, &column("gamersequalizer.exe")).unwrap();
    assert!(mix.score < 0.2, "{mix:?}");
    // The game's streams carried only near-silence: no verdict.
    let mut game = vec![0.0; HOLD_PAD_BEFORE];
    game.extend(column("slot1"));
    game.extend([0.0; HOLD_PAD_AFTER]);
    assert!(hold_match(&game, &column("brave.exe")).is_none());
  }

  #[test]
  fn pick_session_needs_score_and_margin() {
    let m = |score| HoldMatch { score, window: 10, lag: 0, gain: 1.0, points: 100 };
    assert_eq!(pick_session(&[(1, m(0.95)), (2, m(0.05))]).map(|p| p.0), Some(1));
    assert!(pick_session(&[(1, m(0.45))]).is_none(), "score too low");
    assert!(pick_session(&[(1, m(0.9)), (2, m(0.7))]).is_none(), "two sessions explain it");
    assert!(pick_session::<u8>(&[]).is_none());
  }

  #[test]
  fn stream_bus_roundtrip_and_unknown() {
    for bus in [StreamBus::None, StreamBus::Chat, StreamBus::Media] {
      assert_eq!(StreamBus::from_u32(bus.to_u32()), bus);
    }
    assert_eq!(StreamBus::from_u32(9), StreamBus::Unknown(9));
    assert_eq!(StreamBus::Unknown(9).eq_index(), None);
    assert_eq!((StreamBus::Chat.eq_index(), StreamBus::Media.eq_index()), (Some(0), Some(1)));
  }

  /// Deterministic levels in 0.01..1.0 (LCG).
  fn noise(seed: u32, len: usize) -> Vec<f32> {
    let mut x = seed;
    (0..len)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        0.01 + (x >> 8) as f32 / (1u32 << 24) as f32
      })
      .collect()
  }
}
