//! The echo canceller's speaker reference from our output effect: which output it is, whether
//! that output carries the effect, and what the APOs (`aec_ref_output_hash`, `tap_active`) and
//! the window (`aec_ref_status`) are told. The output APO writes the ring itself (`ref_feed.rs`
//! in the APO crate); there is no fallback: an output without the effect gets no echo
//! cancellation, and the window says how to fix it. `spawn` is the thread that drives this and the
//! link to the APOs.

use log::info;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use gamersequalizer_common::shm::{AecRefStatus, ShmHeader};
use gamersequalizer_common::sidetone::device_hash;
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;

use super::apo_install::{self, ApoStatus, Flow};
use super::apo_link::{ApoLink, SYNC_INTERVAL};
use super::endpoint_lock::get_endpoint_info;
use super::{in_use, mmdevice, worker};

/// How often the output is resolved again while none is published (the Windows default: one RPC).
const RESOLVE_INTERVAL: Duration = Duration::from_secs(1);
/// How often the registry is read for the effect on the reference output (or at once on a change).
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A beat of the APO counts as moving while it changed within this time.
const BEAT_WINDOW: Duration = Duration::from_secs(3);

/// State of the reference, ticked by the thread `spawn` starts.
#[derive(Default)]
pub struct OutputReference {
  /// The output in use last seen (None = not published yet: the Windows default) and what it
  /// resolved to.
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

/// Spawn the supervised reference thread: it ticks `OutputReference` and the link to the APOs until
/// `running` is cleared. The output is the one in use (`in_use`), the Windows default until
/// the service loop has published it.
pub fn spawn(running: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name("GamersEqualizer_AecReference".into())
    .spawn(move || {
      // SAFETY: COM init for this thread with no reserved pointer; it stays for the thread's life.
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      let Some(shm) = worker::open_shm("AecRef", || running.load(Ordering::SeqCst)) else {
        return;
      };
      let mut apo = ApoLink::new();
      let mut reference = OutputReference::default();
      let mut idle_log = worker::IdleLog::default();
      info!("[AecRef] reference thread started");
      while running.load(Ordering::SeqCst) {
        // The open window shows the mirrored telemetry and its EQ edits reach the APO through
        // here (one sync per tick at the sync rate): not idle then.
        let busy = reference.tick(&shm, &mut apo);
        let busy = busy.or_else(|| crate::tray::window_visible().then_some(("window open", SYNC_INTERVAL)));
        idle_log.idle("reference thread", busy);
        // Known limit: when an app opens the mic, the echo canceller gets its reference one tick
        // after the DSP watch sees its telemetry, plus one sync (100 ms): ~100-200 ms, from idle
        // up to ~350 ms (the idle tick is 250 ms, on the wake grid).
        worker::sleep(worker::tick_wait(super::ducking::mix_fast(), busy));
      }
      let h = &shm.layout().header;
      h.tap_active.store(false, Ordering::Relaxed);
      h.aec_ref_output_hash.store(0, Ordering::Relaxed);
      apo.set_tap_active(false);
      info!("[AecRef] reference thread stopped");
    })
    .ok()
}

impl OutputReference {
  /// One step: resolve the output, check its effect on a timer, publish the result into the
  /// daemon's mapping (`local`), then let `apo` mirror it into the APO's. Returns why the next
  /// tick must come soon and how soon, None when idle. Only mix mode reads the reference every
  /// tick (`tick_mix`); the mic chain's link and Automatic ChatMix's stream mode live in `apo.tick`,
  /// which runs once per `SYNC_INTERVAL`, so a faster tick would only wake the CPU for nothing.
  pub fn tick(&mut self, local: &SharedMemoryClient, apo: &mut ApoLink) -> Option<worker::Busy> {
    let now = Instant::now();
    let wanted = in_use::output();
    if wanted != self.selection || self.resolved_at.is_none_or(|t| now.duration_since(t) >= RESOLVE_INTERVAL) {
      self.resolved_at = Some(now);
      let render_id = mmdevice::resolve_render_id(wanted.as_deref());
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
    let aec_on = h.aec_enabled.load(Ordering::Relaxed);
    let wanted = reference_wanted(apo.mic_dsp_live(), aec_on, apo.mix_wants_reference());
    let hash = self.render_id.as_deref().filter(|_| installed && wanted).map_or(0, device_hash);
    let tap_active = status == AecRefStatus::OutputEffect;
    publish(h, hash, status, tap_active);
    apo.set_render_device(self.render_id.clone());
    apo.tick_mix(local);
    apo.tick(local, tap_active);
    self.log_change(status);
    // After `apo.tick`: a mic chain that just started makes the next wait short.
    let duck_playing = h.duck_enabled.load(Ordering::Relaxed) && super::stream_router::game_session_playing();
    worker::busy_reason(&[
      (apo.mix_wants_reference(), "mix mode needs the speaker reference", worker::TICK),
      (apo.mic_dsp_live(), "an app records through the mic filter", SYNC_INTERVAL),
      (duck_playing, "Automatic ChatMix with a game playing", SYNC_INTERVAL),
    ])
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

/// Whether the output APO should write the reference ring. Its readers: the mic chain's echo
/// canceller, which reads it only while it is on (with it off the output effect would fold and
/// resample every block for nobody), and Automatic ChatMix's mix mode.
fn reference_wanted(mic_dsp_live: bool, aec_on: bool, mix_wants: bool) -> bool {
  (mic_dsp_live && aec_on) || mix_wants
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
  // SAFETY: COM is initialized on the reference thread that calls this; `device` is live.
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
  fn the_reference_is_written_only_for_a_reader() {
    assert!(reference_wanted(true, true, false), "an app records with echo cancellation on");
    assert!(!reference_wanted(true, false, false), "echo cancellation off: the mic chain never reads it");
    assert!(reference_wanted(true, false, true), "mix mode reads it whatever the mic chain does");
    assert!(reference_wanted(false, false, true));
    assert!(!reference_wanted(false, true, false), "nobody records");
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
