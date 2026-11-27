//! The background service (`--daemon`): devices, mixer, game detection, health, sidetone.

use super::*;
use gamersequalizer_common::shm::ShmHeader;
use log::warn;
use std::sync::atomic::AtomicU32;

/// Reading Windows volumes and re-applying per-app session volumes walks devices and every
/// audio session over COM (opening each app's process for its name). At the loop's 20 Hz
/// that was most of the daemon's CPU. Volume changes are picked up within 250 ms; channel
/// sliders apply at once, a newly started app gets its channel volume within 1 s.
const VOLUME_SYNC_INTERVAL: Duration = Duration::from_millis(250);
/// The same reads (Windows volumes, default devices) while the loop is idle (`worker::busy_reason`):
/// a volume changed outside the app (Windows mixer, keyboard keys) then shows within ~1 s. A
/// device or default change still arrives at once (`poll_device_changed`).
const IDLE_VOLUME_SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// `$e`; with the `cpu-stats` feature its CPU time counts as section `$name` of the loop
/// (`cpu_stats::LoopSections`). Without the feature it is `$e` alone.
macro_rules! timed {
  ($sections:ident, $name:literal, $e:expr) => {{
    #[cfg(feature = "cpu-stats")]
    let started = $sections.start();
    let value = $e;
    #[cfg(feature = "cpu-stats")]
    $sections.stop($name, started);
    value
  }};
}

/// Windows volumes and default devices are read again. `since`: time since the last read, None
/// when there was none or the loop just turned busy (the reads then follow at once).
fn volume_sync_due(since: Option<Duration>, idle: bool) -> bool {
  win_audio::worker::due(since, if idle { IDLE_VOLUME_SYNC_INTERVAL } else { VOLUME_SYNC_INTERVAL })
}

/// `device_ids::heal` over all connected endpoints, then tells the running processes: the
/// picked mic moves to its new id in shared memory, the output EQ reloads its settings.
fn heal_device_ids(win_audio: &WinAudioSystem, profiles_dir: &Path, shm_ptr: Option<NonNull<ShmLayout>>) {
  let mut connected = win_audio.endpoint_lock.enumerate_playback_devices();
  connected.extend(win_audio.endpoint_lock.enumerate_capture_devices());
  let moves = win_audio::device_ids::heal(profiles_dir, &connected);
  let Some(ptr) = shm_ptr.filter(|_| !moves.is_empty()) else { return };
  // SAFETY: the mapping lives until the service exits; no other reference to the header is
  // held while this runs (callers take theirs afterwards), the same as at startup.
  let header = unsafe { &mut (*ptr.as_ptr()).header };
  if let Some((_, new)) = header.get_selected_mic_id().and_then(|cur| moves.iter().find(|(old, _)| *old == cur)) {
    header.set_selected_mic_id(new);
  }
  header.out_settings_version.fetch_add(1, Ordering::AcqRel);
}

/// Game, chat, media, mic and ChatMix as the window's mixer shows them, for the tray flyout's rows:
/// with the voice leveler on the mic's row is its target loudness (not the volume), with Automatic ChatMix on its strength.
fn flyout_channels(h: &ShmHeader) -> Vec<(crate::flyout::MixChannel, crate::flyout::ChannelLevel)> {
  use crate::flyout::{ChannelLevel, MixChannel, balance_slider, loudness_slider};
  let r = Ordering::Relaxed;
  let level =
    |v: &AtomicU32, m: &AtomicBool, max: f32| ChannelLevel { volume: v.load(r) as f32 / 1000.0, muted: m.load(r), max };
  let mic = if h.mic_leveler_enabled.load(r) {
    // The leveler's target on the slider's 0..1 (its range); the icon mutes the mic.
    let target = h.mic_leveler_target_db_x10.load(r) as f32 / 10.0;
    let volume = loudness_slider(target);
    (MixChannel::MicLoudness, ChannelLevel { volume, muted: h.mic_muted.load(r), max: 1.0 })
  } else {
    (MixChannel::Mic, level(&h.mic_volume_permille, &h.mic_muted, ui::state::mic_volume_max(Some(h))))
  };
  let chatmix = if h.duck_enabled.load(r) {
    (
      MixChannel::DuckStrength,
      ChannelLevel { volume: h.duck_strength_permille.load(r) as f32 / 1000.0, muted: false, max: 1.0 },
    )
  } else {
    let balance = h.chatmix_balance_permille.load(r) as f32 / 1000.0;
    (MixChannel::Balance, ChannelLevel { volume: balance_slider(balance), muted: false, max: 1.0 })
  };
  vec![
    (MixChannel::Game, level(&h.game_volume_permille, &h.game_muted, 1.0)),
    (MixChannel::Chat, level(&h.chat_volume_permille, &h.chat_muted, 1.0)),
    (MixChannel::Media, level(&h.media_volume_permille, &h.media_muted, 1.0)),
    mic,
    chatmix,
  ]
}

/// A channel row of the tray flyout moved: what the window's slider does, with the window open or
/// closed. The other channels start from the header (the window may have moved one since this
/// loop read it), so the window, adopting all four, loses nothing. The service's own loop then
/// applies the levels to the sessions; channels with an output of their own and the mic are set
/// here (`apply_mix`, as the window does).
fn set_channel(
  mix: &mut win_audio::ChatMixController,
  h: &ShmHeader,
  channel: crate::flyout::MixChannel,
  volume: f32,
  muted: bool,
) {
  use crate::flyout::MixChannel;
  let r = Ordering::Relaxed;
  let level = |v: &AtomicU32| v.load(r) as f32 / 1000.0;
  (mix.game.volume, mix.game.muted) = (level(&h.game_volume_permille), h.game_muted.load(r));
  (mix.chat.volume, mix.chat.muted) = (level(&h.chat_volume_permille), h.chat_muted.load(r));
  (mix.media.volume, mix.media.muted) = (level(&h.media_volume_permille), h.media_muted.load(r));
  // With the leveler on the mic's volume is the Windows one (`sync_mic`): the header's copy may be
  // older, and `apply_mix` would set it back over an app's or the automatic gain's move.
  if !h.mic_leveler_enabled.load(r) {
    mix.mic.volume = level(&h.mic_volume_permille);
  }
  mix.mic.muted = h.mic_muted.load(r);
  mix.balance = h.chatmix_balance_permille.load(r) as f32 / 1000.0;
  debug!("[Flyout] {channel:?} at {:.0} %, muted {muted}", volume * 100.0);
  // A row read before the leveler or Automatic ChatMix was switched means another control now.
  if !flyout_channels(h).iter().any(|(c, _)| *c == channel) {
    debug!("[Flyout] {channel:?} is no longer shown, change dropped");
    return;
  }
  let volume = Some(volume).filter(|v| v.is_finite());
  let (c, volume_at, muted_at) = match channel {
    MixChannel::Game => (&mut mix.game, &h.game_volume_permille, &h.game_muted),
    MixChannel::Chat => (&mut mix.chat, &h.chat_volume_permille, &h.chat_muted),
    MixChannel::Media => (&mut mix.media, &h.media_volume_permille, &h.media_muted),
    MixChannel::Mic => (&mut mix.mic, &h.mic_volume_permille, &h.mic_muted),
    MixChannel::MicLoudness => {
      if let Some(v) = volume {
        crate::flyout::set_leveler_target(h, v);
      }
      // The mic's own volume stays; only its mute follows the row's icon.
      mix.mic.muted = muted;
      mix.mic.write_to(&h.mic_volume_permille, &h.mic_muted);
      mix.apply_mix();
      return;
    }
    MixChannel::Balance => {
      if let Some(v) = volume {
        mix.set_balance(crate::flyout::balance_value(v));
        h.chatmix_balance_permille.store((mix.balance * 1000.0).round() as i32, r);
      }
      return;
    }
    // Atomics the window never writes back: it reads them (and saves them) as they are.
    MixChannel::DuckStrength => {
      if let Some(v) = volume {
        h.duck_strength_permille.store((v.clamp(0.0, 1.0) * 1000.0).round() as u32, r);
      }
      return;
    }
  };
  c.volume = match (channel, volume) {
    (_, None) => c.volume,
    (MixChannel::Mic, Some(v)) => v.clamp(0.0, ui::state::mic_volume_max(Some(h))),
    (_, Some(v)) => v.clamp(0.0, 1.0),
  };
  c.muted = muted;
  c.write_to(volume_at, muted_at);
  mix.apply_mix();
}

/// Headless background audio service (runs on Windows boot or detached)
pub(super) fn run_background_daemon() {
  info!("[GamersEqualizer] Initializing headless background service, version {}...", env!("GAMERSEQUALIZER_VERSION"));

  // Single-instance daemon guard via named Win32 mutex
  let mutex_name = w!(r"Local\GamersEqualizer_Daemon_Mutex");
  // SAFETY: a static name and no security attributes; the handle is closed at the end of this function.
  let mutex = unsafe { CreateMutexW(None, true, mutex_name) };
  // Access denied: it exists too, held by a service with more rights (see `daemon_running`). Read
  // from the returned error: building it may already have changed the thread's last error.
  let denied = mutex.as_ref().is_err_and(|e| e.code() == ERROR_ACCESS_DENIED.to_hresult());
  // SAFETY: reads this thread's last error, set by the CreateMutexW call above when it succeeded.
  if denied || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
    info!("[GamersEqualizer] Background daemon is already active. Exiting duplicate process.");
    return;
  }
  debug!("[GamersEqualizer] Acquired daemon mutex.");

  // SAFETY: COM init for this thread with no reserved pointer; the service thread keeps it for its life.
  unsafe {
    let res = CoInitializeEx(None, COINIT_MULTITHREADED);
    debug!("[GamersEqualizer] CoInitializeEx: {:?}", res);
  }

  let storage = ProfileStorage::default_storage();
  win_audio::app_channels::init(storage.base_dir());
  win_audio::sidetone::init(storage.base_dir());
  debug!("[GamersEqualizer] Loaded storage.");

  let mut win_audio = match WinAudioSystem::new() {
    Ok(w) => w,
    Err(e) => {
      error!("[GamersEqualizer] Daemon failed to initialize WinAudio: {}", e);
      return;
    }
  };
  debug!("[GamersEqualizer] Initialized WinAudio.");
  // Pairs per-stream APO instances with apps (chat / media stream EQ); runs for the daemon's life.
  if win_audio::stream_router::spawn().is_none() {
    error!("[GamersEqualizer] could not start the stream router: chat/media EQ stays off");
  }

  // Open or create shared memory for IPC
  let (shm_handle, shm_ptr) = shm_mapping::open_or_create_shm(storage.base_dir());
  let mut last_mic_version = 0;
  let mut last_default_mic: Option<String> = None;
  let mut last_default_out: Option<String> = None;
  // Before anything reads a device id: settings follow devices that got a new id meanwhile.
  heal_device_ids(&win_audio, storage.base_dir(), shm_ptr);
  if let Some(ptr) = shm_ptr {
    // SAFETY: the mapping stays mapped until the service exits (unmapped after the loop), and no other reference to
    // the header is held while this block runs, as in `heal_device_ids`.
    let header = unsafe { &mut (*ptr.as_ptr()).header };
    header.daemon_active.store(true, Ordering::SeqCst);
    header.shutdown_daemon_signal.store(false, Ordering::SeqCst);

    let devices = win_audio.endpoint_lock.enumerate_playback_devices();
    let def_id = win_audio.endpoint_lock.get_default_output_id();
    win_audio.chatmix.bind_endpoints(&devices, def_id.as_deref());
    last_default_out = def_id;
    // The mic the user picked, if it is still connected (its own volume, not the default's).
    let captures = win_audio.endpoint_lock.enumerate_capture_devices();
    win_audio.chatmix.use_mic(header.get_selected_mic_id().filter(|id| captures.iter().any(|d| &d.id == id)));
    // Saved (or, if the window runs, current) channel levels instead of 100 %.
    ui::audio_settings::restore_mixer(header, &mut win_audio.chatmix, false);
    last_mic_version = header.mic_id_version.load(Ordering::Relaxed);
    last_default_mic = win_audio.endpoint_lock.get_default_capture_id();

    win_audio.chatmix.write_levels(header);
  }
  // The microphone and output just bound, for the threads below; the loop republishes them.
  win_audio::in_use::publish(&win_audio.chatmix.mic_device_id, &win_audio.chatmix.master_device_id);
  if win_audio::sidetone::spawn().is_none() {
    error!("[GamersEqualizer] could not start the sidetone thread: sidetone stays off");
  }

  // Of the window's state the service needs the output EQ, the mic EQ and the saved audio
  // settings; `AppState` would also load the window's parts and map the shared memory again.
  i18n::set_language(ui::prefs::UiPrefs::load_or_default(storage.base_dir()).language);
  let mut output_eq = ui::state::load_output_eq(&storage, true);
  let mut audio_settings_saved = ui::audio_settings::AudioSettings::default();
  if let Some(ptr) = shm_ptr {
    // SAFETY: the mapping stays mapped until the service exits (unmapped after the loop); atomics only.
    let header = unsafe { &ptr.as_ref().header };
    audio_settings_saved = ui::audio_settings::AudioSettings::from_header(header);
    let mic_presets = ui::mic_presets::all_mic_presets(&storage);
    header.write_mic_eq(&ui::mic_presets::startup_mic_profile(&storage, &mic_presets).bands);
  }
  // SAFETY: as above.
  output_eq.apply(shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header }));

  // Speaker reference for the echo canceller (our output effect on the Master output writes it),
  // plus the link to the microphone APO.
  let reference_running = Arc::new(AtomicBool::new(true));
  let reference_thread = win_audio::aec_reference::spawn(reference_running.clone());

  let mut last_volume_sync: Option<Instant> = None;
  let mut last_output_check: Option<Instant> = None;
  let mut last_mic_link: Option<Instant> = None;
  // Set at the end of each pass: whether the loop was idle (`worker::busy_reason`).
  let mut idle = false;
  // Windows mic volume as the leveler's input gain, stepped at the extremes only.
  let mut mic_gain = win_audio::mic_gain::MicGainLink::default();
  // Who moved the mic volume; holds the leveler while an app's own gain control moves it.
  let mut mic_hold = win_audio::mic_hold::MicHoldLink::default();
  // Debug lines for every leveler change and its reason (written only with "save debug" on).
  let mut leveler_log = win_audio::leveler_log::LevelerLog::default();
  // Debug lines for the whole mic chain: switches, devices, volumes, a 10 s summary.
  let mut chain_log = win_audio::chain_log::ChainLog::default();

  // Notification-area icon: the window "hides to tray" by closing, the service keeps the icon.
  let mut tray_host = tray::TrayHost::new(storage.base_dir());
  // Setup problems (default output / spatial sound lost, mic processing) for the tray badge.
  let mut health = health::HealthMonitor::new(storage.base_dir());
  // Automatic ChatMix's mix mode lowers Game sessions; what an earlier run left lowered (a crash,
  // a game that exited) is set back when its sessions appear.
  win_audio.chatmix.game_duck = win_audio::chatmix::GameDuck::for_service(storage.base_dir());
  let mut idle_log = win_audio::worker::IdleLog::default();
  // The tray flyout's channel changes, for the window to adopt.
  let mixer_edits = crate::mixer_edits::MixerEditsMap::open()
    .inspect_err(|e| warn!("[Flyout] the window will not see channel changes from the tray: {e}"))
    .ok();
  #[cfg(feature = "cpu-stats")]
  let mut sections = crate::cpu_stats::LoopSections::begin();

  info!("[GamersEqualizer] Background service running smoothly. Entering audio monitoring loop...");

  loop {
    // SAFETY: the mapping stays mapped until the service exits (unmapped after this loop); atomics only.
    let tray_header = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header });
    crate::flyout::set_mixer(win_audio.chatmix.master_device_id.as_deref(), tray_header.map(flyout_channels));
    match timed!(sections, "tray_poll", tray_host.poll(tray_header)) {
      Some(tray::TrayAction::Exit) => {
        info!("[GamersEqualizer] Exit chosen in the tray. Closing the window and the service.");
        // Close the window first, or its watchdog would start the service again.
        tray::close_window();
        if let Some(ptr) = shm_ptr {
          // SAFETY: the mapping stays mapped until the service exits (unmapped after this loop); atomics only.
          unsafe { ptr.as_ref() }.header.daemon_active.store(false, Ordering::SeqCst);
        }
        break;
      }
      Some(tray::TrayAction::Fix(fix)) => health.apply(&fix, &win_audio),
      None => {}
    }
    for change in crate::flyout::take_changes() {
      match change {
        crate::flyout::Change::Volume { exe, volume, save } => {
          win_audio.chatmix.set_app_heard_volume(&exe, volume, save)
        }
        crate::flyout::Change::Mute { exe, muted } => win_audio.chatmix.set_app_mute(&exe, muted),
        crate::flyout::Change::Output(id) => {
          // Told before the switch: a window that locks the output moves its lock there instead of
          // switching Windows back.
          if let Some(map) = &mixer_edits {
            map.edits().publish_output(&id);
          }
          win_audio.select_output(storage.base_dir(), &id);
        }
        crate::flyout::Change::Channel { channel, volume, muted, save } => {
          // SAFETY: the mapping stays mapped until the service exits (unmapped after this loop); atomics only.
          if let Some(header) = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header }) {
            set_channel(&mut win_audio.chatmix, header, channel, volume, muted);
            if let Some(map) = &mixer_edits {
              map.edits().publish(&win_audio.chatmix);
            }
            if save {
              audio_settings_saved
                .save_if_changed(ui::audio_settings::AudioSettings::from_header(header), storage.base_dir());
            }
          }
        }
      }
    }
    // A device came or went: the picked mic may have been unplugged (the default does not
    // change when it was not the default) or plugged back in, maybe on another USB port.
    let devices_changed = timed!(sections, "poll_device_changed", win_audio.endpoint_lock.poll_device_changed());
    if devices_changed {
      timed!(sections, "heal_device_ids", heal_device_ids(&win_audio, storage.base_dir(), shm_ptr));
      win_audio.chatmix.outputs_changed();
    }
    // SAFETY: the mapping stays mapped until the service exits (unmapped after this loop);
    // the header is only read through atomics.
    let header = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header });
    if let Some(issues) = timed!(sections, "health", health.tick(&win_audio, header)) {
      tray_host.set_issues(issues);
    }

    // Check shutdown signal from UI or user
    if let Some(ptr) = shm_ptr {
      // SAFETY: as above: still mapped, atomics only.
      let header = unsafe { &ptr.as_ref().header };
      if header.shutdown_daemon_signal.load(Ordering::Relaxed) {
        info!("[GamersEqualizer] Shutdown signal received. Exiting daemon.");
        header.daemon_active.store(false, Ordering::SeqCst);
        break;
      }

      // The mic: the user's pick (from the window) while it is connected, else the Windows
      // default. Re-resolved when the pick or the default changes; the default is polled with
      // the output check below (one COM call per 250 ms, per 1 s idle) and read on a change.
      let cur_mic_ver = header.mic_id_version.load(Ordering::Relaxed);
      let output_check_due = devices_changed || volume_sync_due(last_output_check.map(|t| t.elapsed()), idle);
      let default_mic = if output_check_due {
        timed!(sections, "default_mic", win_audio.endpoint_lock.get_default_capture_id())
      } else {
        last_default_mic.clone()
      };
      // Sonar / VB-Cable installed or started after the service: their channel outputs take
      // over from per-app volumes (and give them back when they go).
      #[cfg(feature = "cpu-stats")]
      let started = sections.start();
      if devices_changed {
        let devices = win_audio.endpoint_lock.enumerate_playback_devices();
        let mix = &mut win_audio.chatmix;
        if mix.bind_channel_endpoints(&devices) {
          info!(
            "[GamersEqualizer] channel outputs now game {:?}, chat {:?}, media {:?}",
            mix.game_device_id, mix.chat_device_id, mix.media_device_id
          );
          // A newly bound channel shows its endpoint's level, not the per-app one it had.
          mix.game.write_to(&header.game_volume_permille, &header.game_muted);
          mix.chat.write_to(&header.chat_volume_permille, &header.chat_muted);
          mix.media.write_to(&header.media_volume_permille, &header.media_muted);
        }
      }
      if cur_mic_ver != last_mic_version || default_mic != last_default_mic || devices_changed {
        last_mic_version = cur_mic_ver;
        last_default_mic = default_mic;
        let captures = win_audio.endpoint_lock.enumerate_capture_devices();
        let picked = header.get_selected_mic_id().filter(|id| captures.iter().any(|d| &d.id == id));
        let wanted = picked.or_else(win_audio::ChatMixController::find_physical_microphone);
        if wanted.is_some() && wanted != win_audio.chatmix.mic_device_id {
          info!("[GamersEqualizer] microphone now {:?}", wanted);
          mic_gain.switch_mic(&mut win_audio.chatmix);
          mic_hold.switch_mic();
          win_audio.chatmix.use_mic(wanted);
        }
      }
      #[cfg(feature = "cpu-stats")]
      sections.stop("device_follow", started);

      // The Windows default output changed (in Windows, in the window, in another app), or a
      // device came or went (the headset behind Sonar unplugged: Master kept a dead id): the
      // Master channel and the echo reference follow. With the default unchanged, Master stays
      // on its device while that is connected (`master_endpoint_id`). Only the output is
      // re-resolved; the mic choice and levels stay (`bind_endpoints` would reset them).
      if output_check_due {
        last_output_check = Some(Instant::now());
        let default_out = timed!(sections, "default_out", win_audio.endpoint_lock.get_default_output_id());
        if default_out.is_some() && (devices_changed || default_out != last_default_out) {
          let devices = win_audio.endpoint_lock.enumerate_playback_devices();
          let current = win_audio.chatmix.master_device_id.as_deref().filter(|_| default_out == last_default_out);
          let master = win_audio::chatmix::master_endpoint_id(&devices, default_out.as_deref(), current);
          if master != win_audio.chatmix.master_device_id {
            info!("[GamersEqualizer] default output changed: Master and echo reference now {:?}", master);
            win_audio.chatmix.master_device_id = master;
          }
          last_default_out = default_out;
        }
      }
      // Sidetone, the echo reference and the APO link follow the mic and output resolved above.
      timed!(
        sections,
        "in_use",
        win_audio::in_use::publish(&win_audio.chatmix.mic_device_id, &win_audio.chatmix.master_device_id)
      );

      // 1. Sync live Windows master & channel volumes to daemon & SHM (slower while idle). The mic
      // volume link and the chain logs stay at 250 ms: the gain step counts clips per tick.
      let mic_link_due = win_audio::worker::due(last_mic_link.map(|t| t.elapsed()), VOLUME_SYNC_INTERVAL);
      if mic_link_due {
        last_mic_link = Some(Instant::now());
        // Leveler off: the wish (may be a boost > 100 %) is what the window wrote; Windows only
        // shows <= 100 %. Leveler on: the Windows volume (`sync_mic`), the window's copy may be older.
        if !header.mic_leveler_enabled.load(Ordering::Relaxed) {
          win_audio.chatmix.mic.volume = header.mic_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
        }
      }
      let synced = volume_sync_due(last_volume_sync.map(|t| t.elapsed()), idle);
      if synced {
        last_volume_sync = Some(Instant::now());
        if timed!(sections, "sync_from_system", win_audio.chatmix.sync_from_system()) {
          timed!(sections, "write_levels", win_audio.chatmix.write_levels(header));
        }
      }
      if mic_link_due {
        let mic_id = win_audio.chatmix.mic_device_id.clone();
        // The mic's volume at the link's pace also while the full sync above waits (idle).
        if !synced && timed!(sections, "sync_mic", win_audio.chatmix.sync_mic()) {
          timed!(sections, "write_levels", win_audio.chatmix.write_levels(header));
        }
        let foreground = || win_audio.game_detector.as_ref().and_then(|d| d.current()).map(|e| e.exe_name);
        timed!(
          sections,
          "mic_hold",
          mic_hold.tick(header, &mut win_audio.chatmix, mic_id.as_deref(), foreground, storage.base_dir())
        );
        timed!(
          sections,
          "mic_gain",
          mic_gain.tick(header, &mut win_audio.chatmix, mic_id.as_deref(), storage.base_dir())
        );
        timed!(sections, "leveler_log", leveler_log.tick(header));
        timed!(
          sections,
          "chain_log",
          chain_log.tick(header, || {
            win_audio::chain_log::read_context(&win_audio.chatmix, &win_audio.endpoint_lock, mic_id.as_deref())
          })
        );
      }

      // 2. Read sub-bus levels from SHM for background session scaling
      let g_vol = header.game_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      let c_vol = header.chat_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      let med_vol = header.media_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;

      let g_mute = header.game_muted.load(Ordering::Relaxed);
      let c_mute = header.chat_muted.load(Ordering::Relaxed);
      let med_mute = header.media_muted.load(Ordering::Relaxed);

      let bal = (header.chatmix_balance_permille.load(Ordering::Relaxed) as f32) / 1000.0;

      win_audio.chatmix.game.volume = g_vol;
      win_audio.chatmix.chat.volume = c_vol;
      win_audio.chatmix.media.volume = med_vol;

      win_audio.chatmix.game.muted = g_mute;
      win_audio.chatmix.chat.muted = c_mute;
      win_audio.chatmix.media.muted = med_mute;

      win_audio.chatmix.balance = bal;
      // Atomics only: automatic ChatMix in the APO weighs game against chat as they are heard.
      timed!(sections, "write_duck_gains", win_audio.chatmix.write_duck_gains(header));
      // Mix mode lowers the Game sessions through the one writer of them (`apply_session_mix`).
      timed!(
        sections,
        "set_session_mode",
        win_audio::ducking::set_session_mode(
          win_audio.chatmix.owns_session_volumes(),
          win_audio.chatmix.get_multipliers(),
        )
      );
      win_audio.chatmix.duck_factor = win_audio::ducking::game_duck_factor();
      win_audio.chatmix.duck_subject = win_audio::ducking::mix_subject();

      // Per-app session volumes: fallback mode only, throttled, never against the user's
      // own mixer changes (`apply_session_mix`). Idle: new sessions come from Windows' own
      // announcement, the walk over every session is rare.
      win_audio.chatmix.sessions_idle = idle;
      timed!(sections, "apply_session_mix", win_audio.chatmix.apply_session_mix());
    }

    // SAFETY: still mapped, atomics only (as above).
    let header = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header });
    // Game detection: the daemon is the owner (decides and publishes the active game).
    timed!(sections, "game_detector", {
      if let Some(ref detector) = win_audio.game_detector {
        while let Some(event) = detector.poll_event() {
          output_eq.on_foreground(&event, true, header);
        }
      }
    });
    timed!(sections, "output_eq", output_eq.tick(true, win_audio.chatmix.master_device_id.clone(), header));

    // Idle: Automatic ChatMix off or without a game's session (session ducking and mix mode run
    // through here and lower games only; the router finds a new session within 500 ms, and pairing
    // its stream takes ~1.5 s anyway) and the window closed (its sliders reach the per-app session volumes through here).
    // An app that records needs no fast pass: the tray's once-a-second check and frame, the mic
    // volume link and the chain logs (250 ms) fit the idle tick, and the level meter's peak is kept
    // by the telemetry mirror. The tray icon stays quick: its messages end the wait, and so does
    // Windows announcing that an app opened or closed a microphone (one pass, the loop stays idle).
    let duck_playing = header.is_some_and(|h| h.duck_enabled.load(Ordering::Relaxed))
      && win_audio::stream_router::game_session_playing();
    let busy = win_audio::worker::busy_reason(&[
      (timed!(sections, "find_window", tray::window_visible()), "window open", win_audio::worker::TICK),
      (duck_playing, "Automatic ChatMix with a game playing", win_audio::worker::TICK),
    ]);
    let was_idle = std::mem::replace(&mut idle, idle_log.idle("service loop", busy));
    if was_idle && !idle {
      // Turned busy (the window opened, ...): read volumes and default devices on the next pass.
      (last_volume_sync, last_output_check) = (None, None);
    }
    let wait = win_audio::worker::tick_wait(win_audio::ducking::mix_fast(), busy);
    // Idle: on the wake grid with the other service threads (`worker::grid_wait`).
    // The open flyout too: its drags and keys must not wait for a plain sleep.
    let on_messages = idle || crate::flyout::is_open();
    timed!(sections, "wait", if on_messages { tray_host.wait(wait) } else { win_audio::worker::sleep(wait) });
  }

  // A game lowered by mix mode gets its volume back before the service goes (or, if that fails,
  // at the next start from `ducking_lowered.json`).
  win_audio.chatmix.release_game_sessions();

  reference_running.store(false, Ordering::SeqCst);
  if let Some(handle) = reference_thread {
    let _ = handle.join();
  }
  if let Some(ptr) = shm_ptr {
    // SAFETY: the view from `open_or_create_shm`, unmapped once here; nothing uses `shm_ptr` after the loop
    // (the threads have mappings of their own).
    unsafe {
      let _ =
        UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.as_ptr() as *mut _ });
    }
  }
  if let Some(h) = shm_handle {
    // SAFETY: the mapping handle from `open_or_create_shm`, closed once here.
    unsafe {
      let _ = CloseHandle(h);
    }
  }
  if let Ok(m) = mutex {
    // SAFETY: the mutex handle from the start of this function, closed once here.
    unsafe {
      let _ = CloseHandle(m);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn idle_reads_windows_once_a_second_and_busy_at_once() {
    let half = Duration::from_millis(500);
    assert!(volume_sync_due(None, true), "never read, or just turned busy: read now");
    assert!(volume_sync_due(None, false));
    assert!(volume_sync_due(Some(half), false), "busy: every 250 ms");
    assert!(!volume_sync_due(Some(half), true), "idle: not before 1 s");
    assert!(volume_sync_due(Some(IDLE_VOLUME_SYNC_INTERVAL), true));
    assert!(!volume_sync_due(Some(Duration::from_millis(100)), false));
  }

  #[test]
  fn the_flyout_row_sets_the_leveler_target_in_whole_db() {
    let h = ShmHeader::default();
    // (slider, target x10): -36..-12 dBFS over 0..1; a wheel notch (0.02 = 0.48 dB) lands on a whole dB.
    for (slider, target_x10) in [(0.75, -180), (0.52, -240), (0.0, -360), (2.0, -120), (-1.0, -360)] {
      crate::flyout::set_leveler_target(&h, slider);
      assert_eq!(h.mic_leveler_target_db_x10.load(Ordering::Relaxed), target_x10, "slider {slider}");
    }
  }

  #[test]
  fn the_flyout_gets_the_mic_and_chatmix_controls_that_are_on() {
    use crate::flyout::MixChannel;
    let h = ShmHeader::default();
    let r = Ordering::Relaxed;
    let kinds = |h: &ShmHeader| flyout_channels(h).iter().skip(3).map(|(c, _)| *c).collect::<Vec<_>>();
    h.mic_leveler_enabled.store(false, r);
    h.duck_enabled.store(false, r);
    assert_eq!(kinds(&h), [MixChannel::Mic, MixChannel::Balance]);
    // The balance -0.4 (game louder) is the slider's 0.3, which the service turns back into -0.4.
    h.chatmix_balance_permille.store(-400, r);
    let (_, balance) = *flyout_channels(&h).last().unwrap();
    assert!((balance.volume - 0.3).abs() < 1e-6 && (balance.volume * 2.0 - 1.0 + 0.4).abs() < 1e-6);
    h.mic_leveler_enabled.store(true, r);
    h.duck_enabled.store(true, r);
    h.duck_strength_permille.store(700, r);
    assert_eq!(kinds(&h), [MixChannel::MicLoudness, MixChannel::DuckStrength]);
    assert_eq!(flyout_channels(&h).last().unwrap().1.volume, 0.7);
    // The default target (-24 dBFS) is the middle of -36..-12.
    let (_, loudness) = flyout_channels(&h)[3];
    assert!((loudness.volume - 0.5).abs() < 1e-6);
    // Muted mic: the loudness row shows it, so its icon can unmute.
    assert!(!loudness.muted);
    h.mic_muted.store(true, r);
    assert!(flyout_channels(&h)[3].1.muted);
  }
}
