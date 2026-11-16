//! Capture times: the mic clock steered from QPC timestamps, and the time helpers.

use super::*;

/// `APO_CONNECTION_PROPERTY_V2_SIGNATURE` ('ACP2'): the connection carries a QPC timestamp.
pub(super) const CONNECTION_V2_SIGNATURE: u32 = 0x4143_5032;

const HNS_PER_SEC: i128 = 10_000_000;

/// A V2 timestamp further than this from "now" is ignored (unknown units / garbage).
const MAX_TIMESTAMP_SKEW_HNS: u64 = 10_000_000;

/// The mic clock re-anchors when a block's time is this far off (stream gap / glitch).
const CLOCK_RESYNC_HNS: i64 = 500_000;

/// Fraction of the timestamp error corrected per block (~2.5 s time constant at 10 ms
/// blocks): filters scheduling jitter of the engine's call times.
const CLOCK_CORRECTION_DIVISOR: i64 = 256;

/// Capture time of each block from a sample counter, slowly steered towards the raw
/// timestamps. Raw APO call times jitter by milliseconds, and the delay estimator needs
/// windows that agree within 2 ms, and the AEC needs a steady alignment.
pub(super) struct MicClock {
  /// Device frames per second (what `next` counts in).
  pub(super) rate: u32,
  /// (time of frame 0 in hns, frames since then)
  anchor: Option<(i64, u64)>,
}

impl MicClock {
  pub(super) fn new(rate: u32) -> Self {
    Self { rate, anchor: None }
  }

  /// Smoothed time of this block's first frame; `raw` is its measured time.
  pub(super) fn next(&mut self, raw: Option<u64>, frames: u64) -> Option<u64> {
    let raw = raw.and_then(|t| i64::try_from(t).ok());
    let frames_to_hns = |n| frames_to_hns(n, self.rate);
    let estimate = match (self.anchor, raw) {
      (Some((t0, n)), Some(raw)) => {
        let expected = t0 + frames_to_hns(n);
        let error = raw - expected;
        if error.abs() > CLOCK_RESYNC_HNS {
          self.anchor = Some((raw, 0));
          raw
        } else {
          let t0 = t0 + error / CLOCK_CORRECTION_DIVISOR;
          self.anchor = Some((t0, n));
          t0 + frames_to_hns(n)
        }
      }
      // No measurement this block: keep counting.
      (Some((t0, n)), None) => t0 + frames_to_hns(n),
      (None, Some(raw)) => {
        self.anchor = Some((raw, 0));
        raw
      }
      (None, None) => return None,
    };
    if let Some((t0, n)) = self.anchor {
      self.anchor = Some((t0, n + frames));
    }
    u64::try_from(estimate).ok()
  }
}

pub(super) fn frames_to_hns(frames: u64, rate: u32) -> i64 {
  (frames as i128 * HNS_PER_SEC / rate.max(1) as i128) as i64
}

/// QPC time (100 ns units) of the first frame: the engine's V2 timestamp when present and
/// plausible, else "now minus the buffer length" (the delay estimator absorbs the offset).
///
/// A raw pointer, not a reference: a `&APO_CONNECTION_PROPERTY` covers only the V1 fields, so the
/// V2 timestamp behind them may not be read through it.
///
/// # Safety
/// `input` points to a live connection property, a whole `APO_CONNECTION_PROPERTY_V2` when its
/// signature says so (audio engine contract).
pub(super) unsafe fn capture_time_hns(input: *const APO_CONNECTION_PROPERTY, rate: u32) -> Option<u64> {
  let now = now_hns()?;
  // SAFETY: V1 fields of a live property (contract above).
  let (signature, frames) = unsafe { ((*input).u32Signature, (*input).u32ValidFrameCount) };
  if signature == CONNECTION_V2_SIGNATURE {
    // SAFETY: the V2 signature means the engine passed a whole V2 property (contract above).
    let t = unsafe { addr_of!((*input.cast::<APO_CONNECTION_PROPERTY_V2>()).u64QPCTime).read() };
    if t.abs_diff(now) < MAX_TIMESTAMP_SKEW_HNS {
      return Some(t);
    }
  }
  // 0 = not locked yet (the engine doesn't process then); assume the usual rate.
  let rate = if rate == 0 { REF_SAMPLE_RATE } else { rate };
  let duration = frames_to_hns(u64::from(frames), rate).max(0) as u64;
  Some(now.saturating_sub(duration))
}

pub(super) fn now_hns() -> Option<u64> {
  let (mut counter, mut freq) = (0i64, 0i64);
  // SAFETY: out parameters are locals.
  unsafe {
    QueryPerformanceCounter(&mut counter).ok()?;
    QueryPerformanceFrequency(&mut freq).ok()?;
  }
  if freq <= 0 {
    return None;
  }
  u64::try_from(counter as i128 * HNS_PER_SEC / freq as i128).ok()
}
