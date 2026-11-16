//! Whether an owner is publishing settings right now (`owner_beat`), seen from the APO side.

use super::*;

/// Last `owner_beat` change any `OwnerWatch` in this process saw, in one value so the two halves
/// never come from different watches: the beat in the high half, the low half tenths of a second
/// after `beat_epoch()` plus one (0 = none seen yet; 13 years of range).
static LAST_BEAT_CHANGE: AtomicU64 = AtomicU64::new(0);
static BEAT_EPOCH: OnceLock<Instant> = OnceLock::new();

/// Watches `owner_beat` from the APO side, on this process's clock (no time shared across sessions).
#[derive(Debug, Default)]
pub struct OwnerWatch {
  /// Last beat value and when it last changed (None = not seen changing yet).
  seen: Option<(u32, Option<Instant>)>,
}

impl OwnerWatch {
  /// An owner is publishing settings right now. False until this or an earlier watch in the
  /// process saw the beat change, so a value left behind by an owner that is gone never counts.
  /// A watch for a new stream starts from the earlier one: the stream's first blocks are
  /// processed too, not passed through until the next beat.
  pub fn live(&mut self, h: &ShmHeader, now: Instant) -> bool {
    let beat = h.owner_beat.load(Ordering::Acquire);
    let changed_at = match self.seen {
      None => last_beat_change().filter(|&(b, _)| b == beat).map(|(_, at)| at),
      Some((last, at)) if last == beat => at,
      Some(_) => {
        note_beat_change(beat, now);
        Some(now)
      }
    };
    self.seen = Some((beat, changed_at));
    changed_at.is_some_and(|t| now.saturating_duration_since(t) < OWNER_TIMEOUT)
  }
}

/// Zero point of `LAST_BEAT_CHANGE`; set once, by the first beat change seen.
fn beat_epoch() -> Instant {
  *BEAT_EPOCH.get_or_init(Instant::now)
}

fn note_beat_change(beat: u32, at: Instant) {
  let tenths = at.saturating_duration_since(beat_epoch()).as_millis() / 100;
  let tenths = u32::try_from(tenths).unwrap_or(u32::MAX - 1).min(u32::MAX - 1) + 1;
  LAST_BEAT_CHANGE.store((u64::from(beat) << 32) | u64::from(tenths), Ordering::Relaxed);
}

/// The beat and when it changed, as the last watch in this process saw it.
fn last_beat_change() -> Option<(u32, Instant)> {
  let v = LAST_BEAT_CHANGE.load(Ordering::Relaxed);
  let tenths = v as u32;
  (tenths != 0).then(|| ((v >> 32) as u32, beat_epoch() + Duration::from_millis(u64::from(tenths - 1) * 100)))
}
