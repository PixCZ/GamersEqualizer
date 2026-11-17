//! The echo canceller's speaker reference from our output effect: which output it is, whether
//! that output carries the effect, and what the APOs (`aec_ref_output_hash`, `tap_active`) and
//! the window (`aec_ref_status`) are told. The output APO writes the ring itself (`ref_feed.rs`
//! in the APO crate); there is no fallback: an output without the effect gets no echo
//! cancellation, and the window says how to fix it.

use log::info;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::{AecRefStatus, ShmHeader};
use gamersequalizer_common::sidetone::device_hash;
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;

use super::apo_install::{self, ApoStatus, Flow};
use super::apo_link::ApoLink;
use super::endpoint_lock::get_endpoint_info;
use super::loopback::resolve_render_id;
use super::mmdevice;

/// How often the Windows default output is resolved again (one RPC).
const RESOLVE_INTERVAL: Duration = Duration::from_secs(1);
/// How often the registry is read for the effect on the reference output (or at once on a change).
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A beat of the APO counts as moving while it changed within this time.
const BEAT_WINDOW: Duration = Duration::from_secs(3);

/// State of the APO source, ticked by the loopback thread (`loopback::spawn`).
#[derive(Default)]
pub struct OutputReference {
  /// The user's output selection last seen (None = the Windows default) and what it resolved to.
  selection: Option<String>,
  render_id: Option<String>,
  resolved_at: Option<Instant>,
  /// Our effect on `render_id`, from the registry.
  install: Option<ApoStatus>,
  checked_at: Option<Instant>,
  writer: BeatWatch,
  blind: BeatWatch,
  /// The writer beat moved since `render_id` was named: the output's effect knows its endpoint,
  /// so a blind instance is another output's.
  writer_seen: bool,
  /// What the log said last: (status, output).
  logged: Option<(AecRefStatus, Option<String>)>,
}

/// When an APO beat in the mapping last changed.
#[derive(Default)]
struct BeatWatch {
  /// The beat last seen and when it changed (None = not seen changing yet).
  last: Option<(u32, Option<Instant>)>,
}

impl OutputReference {
  /// One step: resolve the output, check its effect on a timer, publish the result into the
  /// daemon's mapping (`local`), then let `apo` mirror it into the APO's.
  pub fn tick(&mut self, local: &SharedMemoryClient, apo: &mut ApoLink, selection: &Mutex<Option<String>>) {
    let now = Instant::now();
    let wanted = selection.lock().ok().and_then(|g| g.clone());
    if wanted != self.selection || self.resolved_at.is_none_or(|t| now.duration_since(t) >= RESOLVE_INTERVAL) {
      self.resolved_at = Some(now);
      let render_id = resolve_render_id(wanted.as_deref());
      if render_id != self.render_id {
        self.render_id = render_id;
        self.checked_at = None;
        self.writer_seen = false;
      }
      self.selection = wanted;
    }
    if self.checked_at.is_none_or(|t| now.duration_since(t) >= CHECK_INTERVAL) {
      self.checked_at = Some(now);
      self.install = self.render_id.as_deref().map(|id| apo_install::check_flow(Flow::Render, Some(id)).status);
    }

    let h = &local.layout().header;
    // Mirrored from the APO's mapping by `apo.tick` (`copy_output_telemetry_from`).
    self.writer_seen |= self.writer.moving(h.aec_ref_writer_beat.load(Ordering::Relaxed), now);
    let blind = self.blind.moving(h.aec_ref_blind_beat.load(Ordering::Relaxed), now);
    let status = status_of(self.render_id.is_some(), self.install.as_ref(), self.writer_seen, blind);
    // The output is named while the effect is there and the mic chain runs, also when the
    // instance cannot tell its endpoint: its beat is what tells that case apart.
    let installed = matches!(status, AecRefStatus::OutputEffect | AecRefStatus::EndpointUnknown);
    let hash = self.render_id.as_deref().filter(|_| installed && apo.mic_dsp_live()).map_or(0, device_hash);
    let tap_active = status == AecRefStatus::OutputEffect;
    publish(h, hash, status, tap_active);
    apo.set_render_device(self.render_id.clone());
    apo.tick(local, tap_active);
    self.log_change(status);
  }

  /// One `[AecRef]` line per change of the status or the output, with the reason.
  fn log_change(&mut self, status: AecRefStatus) {
    let now = (status, self.render_id.clone());
    if self.logged.as_ref() == Some(&now) {
      return;
    }
    let name = self.render_id.as_deref().map_or_else(|| "?".to_string(), device_name);
    match status {
      AecRefStatus::OutputEffect => info!("[AecRef] source: our output effect on \"{name}\""),
      AecRefStatus::NoOutput => info!("[AecRef] no reference: no output device"),
      AecRefStatus::EffectMissing => info!("[AecRef] no reference: output effect not installed on \"{name}\""),
      AecRefStatus::EffectOutdated => {
        info!("[AecRef] no reference: the output effect on \"{name}\" is an older build, update it")
      }
      AecRefStatus::EndpointUnknown => info!(
        "[AecRef] no reference: Windows did not tell the output effect which device it runs on (\"{name}\"), see [ApoNote]"
      ),
      other => info!("[AecRef] status {other:?} on \"{name}\""),
    }
    self.logged = Some(now);
  }
}

impl BeatWatch {
  /// True while `beat` changed within `BEAT_WINDOW`. The first look only learns the value: a
  /// beat left over from earlier is not movement.
  fn moving(&mut self, beat: u32, now: Instant) -> bool {
    match self.last {
      Some((last, changed)) if last == beat => changed.is_some_and(|at| now.duration_since(at) < BEAT_WINDOW),
      seen => {
        self.last = Some((beat, seen.map(|_| now)));
        seen.is_some()
      }
    }
  }
}

/// The status for the window, from the registry and the APO's beats. The registry decides
/// whether the effect is there; "no writer beat" alone may only mean nothing plays.
/// `writer_seen`: the reference output's effect wrote since it was named; `blind`: an instance
/// that cannot tell its endpoint runs now.
fn status_of(has_output: bool, install: Option<&ApoStatus>, writer_seen: bool, blind: bool) -> AecRefStatus {
  match install {
    _ if !has_output => AecRefStatus::NoOutput,
    None => AecRefStatus::NotChecked,
    Some(ApoStatus::Active) if blind && !writer_seen => AecRefStatus::EndpointUnknown,
    Some(ApoStatus::Active) => AecRefStatus::OutputEffect,
    Some(ApoStatus::Outdated) => AecRefStatus::EffectOutdated,
    // The id has no endpoint GUID: nothing to check.
    Some(ApoStatus::NoMicrophone) => AecRefStatus::NoOutput,
    Some(ApoStatus::NotInstalled | ApoStatus::NotOnThisMic) => AecRefStatus::EffectMissing,
  }
}

/// Into the daemon's mapping; `ApoLink` copies the hash with the controls, and `tap_active`
/// separately (it writes the APO's on every sync).
fn publish(h: &ShmHeader, hash: u32, status: AecRefStatus, tap_active: bool) {
  h.aec_ref_output_hash.store(hash, Ordering::Relaxed);
  h.aec_ref_status.store(status.to_u32(), Ordering::Relaxed);
  h.tap_active.store(tap_active, Ordering::Relaxed);
}

/// Friendly name of an output for the log, or its id when Windows does not give one.
fn device_name(id: &str) -> String {
  let Ok(device) = mmdevice::device(id) else {
    return id.to_string();
  };
  // SAFETY: COM is initialized on the loopback thread that calls this; `device` is live.
  unsafe { get_endpoint_info(&device, "") }.map_or_else(|_| id.to_string(), |info| info.name)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn status_comes_from_the_registry_and_names_the_endpoint_case() {
    let active = Some(&ApoStatus::Active);
    assert_eq!(status_of(false, active, true, false), AecRefStatus::NoOutput);
    assert_eq!(status_of(true, None, false, false), AecRefStatus::NotChecked);
    // Nothing plays (no writer beat): still the output effect, not a missing one.
    assert_eq!(status_of(true, active, false, false), AecRefStatus::OutputEffect);
    assert_eq!(status_of(true, active, false, true), AecRefStatus::EndpointUnknown);
    // Another instance cannot tell its endpoint, but the reference output's effect wrote.
    assert_eq!(status_of(true, active, true, true), AecRefStatus::OutputEffect);
    assert_eq!(status_of(true, Some(&ApoStatus::NotOnThisMic), false, false), AecRefStatus::EffectMissing);
    assert_eq!(status_of(true, Some(&ApoStatus::NotInstalled), false, false), AecRefStatus::EffectMissing);
    assert_eq!(status_of(true, Some(&ApoStatus::Outdated), false, false), AecRefStatus::EffectOutdated);
  }

  #[test]
  fn a_beat_moves_only_after_it_changed() {
    let mut watch = BeatWatch::default();
    let t = Instant::now();
    assert!(!watch.moving(5, t), "a value left over from earlier");
    assert!(!watch.moving(5, t + Duration::from_secs(1)));
    assert!(watch.moving(6, t + Duration::from_secs(2)));
    assert!(watch.moving(6, t + Duration::from_secs(4)));
    assert!(!watch.moving(6, t + Duration::from_secs(6)), "stood still for the window");
  }
}
