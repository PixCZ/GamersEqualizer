//! The background service (`--daemon`): devices, mixer, game detection, health, sidetone.

use super::*;

/// Reading Windows volumes and re-applying per-app session volumes walks devices and every
/// audio session over COM (opening each app's process for its name). At the loop's 20 Hz
/// that was most of the daemon's CPU. Volume changes are picked up within 250 ms; channel
/// sliders apply at once, a newly started app gets its channel volume within 1 s.
const VOLUME_SYNC_INTERVAL: Duration = Duration::from_millis(250);

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
  if win_audio::sidetone::spawn().is_none() {
    error!("[GamersEqualizer] could not start the sidetone thread: sidetone stays off");
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

  // Of the window's state the service needs the output EQ, the mic EQ and the saved audio
  // settings; `AppState` would also load the window's parts and map the shared memory again.
  i18n::set_language(ui::prefs::UiPrefs::load_or_default(storage.base_dir()).language);
  let mut output_eq = ui::state::load_output_eq(&storage);
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
  let echo_reference = Arc::new(Mutex::new(win_audio.chatmix.master_device_id.clone()));
  let reference_thread = win_audio::aec_reference::spawn(reference_running.clone(), echo_reference.clone());

  let mut last_volume_sync: Option<Instant> = None;
  let mut last_output_check: Option<Instant> = None;
  // Windows mic volume as the leveler's input gain, stepped at the extremes only.
  let mut mic_gain = win_audio::mic_gain::MicGainLink::default();
  // Debug lines for every leveler change and its reason (written only with "save debug" on).
  let mut leveler_log = win_audio::leveler_log::LevelerLog::default();
  // Debug lines for the whole mic chain: switches, devices, volumes, a 10 s summary.
  let mut chain_log = win_audio::chain_log::ChainLog::default();

  // Notification-area icon: the window "hides to tray" by closing, the service keeps the icon.
  let mut tray_host = tray::TrayHost::new();
  // Setup problems (default output / spatial sound lost, mic processing) for the tray badge.
  let mut health = health::HealthMonitor::new(storage.base_dir());

  info!("[GamersEqualizer] Background service running smoothly. Entering audio monitoring loop...");

  loop {
    match tray_host.poll() {
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
    // A device came or went: the picked mic may have been unplugged (the default does not
    // change when it was not the default) or plugged back in, maybe on another USB port.
    let devices_changed = win_audio.endpoint_lock.poll_device_changed();
    if devices_changed {
      heal_device_ids(&win_audio, storage.base_dir(), shm_ptr);
    }
    // SAFETY: the mapping stays mapped until the service exits (unmapped after this loop);
    // the header is only read through atomics.
    let header = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header });
    if let Some(issues) = health.tick(&win_audio, header) {
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
      // the output check below (one COM call per 250 ms).
      let cur_mic_ver = header.mic_id_version.load(Ordering::Relaxed);
      let default_mic = if last_output_check.is_none_or(|t| t.elapsed() >= VOLUME_SYNC_INTERVAL) {
        win_audio.endpoint_lock.get_default_capture_id()
      } else {
        last_default_mic.clone()
      };
      // Sonar / VB-Cable installed or started after the service: their channel outputs take
      // over from per-app volumes (and give them back when they go).
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
          // The old mic gets its own wish back before `use_mic` loads the new one's volume.
          let old =
            win_audio.chatmix.mic_device_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
          mic_gain.switch_mic(&mut win_audio.chatmix, old.as_deref());
          win_audio.chatmix.use_mic(wanted);
        }
      }

      // The Windows default output changed (in Windows, in the window, in another app), or a
      // device came or went (the headset behind Sonar unplugged: Master kept a dead id): the
      // Master channel and the echo reference follow. With the default unchanged, Master stays
      // on its device while that is connected (`master_endpoint_id`). Only the output is
      // re-resolved; the mic choice and levels stay (`bind_endpoints` would reset them).
      if devices_changed || last_output_check.is_none_or(|t| t.elapsed() >= VOLUME_SYNC_INTERVAL) {
        last_output_check = Some(Instant::now());
        let default_out = win_audio.endpoint_lock.get_default_output_id();
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
      // Follow output device changes for the echo reference (reopens only on change).
      if let Ok(mut reference) = echo_reference.lock() {
        reference.clone_from(&win_audio.chatmix.master_device_id);
      }

      // 1. Sync live Windows master & channel volumes to daemon & SHM
      let sync_due = last_volume_sync.is_none_or(|t| t.elapsed() >= VOLUME_SYNC_INTERVAL);
      if sync_due {
        last_volume_sync = Some(Instant::now());
        // The wish (may be a boost > 100 %) is what the window wrote; Windows only shows <= 100 %.
        win_audio.chatmix.mic.volume = header.mic_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      }
      if sync_due && win_audio.chatmix.sync_from_system() {
        win_audio.chatmix.write_levels(header);
      }
      if sync_due {
        let mic_id =
          win_audio.chatmix.mic_device_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
        if mic_gain.tick(header, &mut win_audio.chatmix, mic_id.as_deref()) {
          audio_settings_saved
            .save_if_changed(ui::audio_settings::AudioSettings::from_header(header), storage.base_dir());
        }
        leveler_log.tick(header);
        chain_log.tick(header, || {
          win_audio::chain_log::read_context(&win_audio.chatmix, &win_audio.endpoint_lock, mic_id.as_deref())
        });
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

      // Per-app session volumes: fallback mode only, throttled, never against the user's
      // own mixer changes (`apply_session_mix`).
      win_audio.chatmix.apply_session_mix();
    }

    // SAFETY: still mapped, atomics only (as above).
    let header = shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header });
    // Game detection: the daemon is the owner (decides and publishes the active game).
    if let Some(ref detector) = win_audio.game_detector {
      while let Some(event) = detector.poll_event() {
        output_eq.on_foreground(&event, true, header);
      }
    }
    output_eq.tick(true, win_audio.chatmix.master_device_id.clone(), header);

    std::thread::sleep(Duration::from_millis(50));
  }

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
