#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app_icon;
pub mod app_log;
pub mod autostart;
pub mod dsp_apo;
pub mod health;
pub mod i18n;
pub mod i18n_cs;
pub mod mic_usage;
pub mod notify;
pub mod output_eq;
pub mod sonar_importer;
pub mod tray;
pub mod ui;
pub mod uninstall;
pub mod win_audio;

use eframe::egui_wgpu::{self, wgpu};
use log::{debug, error, info};
use sonar_importer::{import_sonar_db, ProfileStorage, DEFAULT_SONAR_DB_PATH};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use ui::{AppState, GamersEqualizerApp};
use win_audio::WinAudioSystem;
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
use windows::Win32::System::Memory::UnmapViewOfFile;
use windows::Win32::System::Threading::CreateMutexW;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const DETACHED_PROCESS: u32 = 0x00000008;
/// Reading Windows volumes and re-applying per-app session volumes walks devices and every
/// audio session over COM (opening each app's process for its name). At the loop's 20 Hz
/// that was most of the daemon's CPU. Volume changes are picked up within 250 ms; channel
/// sliders apply at once, a newly started app gets its channel volume within 1 s.
const VOLUME_SYNC_INTERVAL: Duration = Duration::from_millis(250);
/// Window / taskbar icon resolution (Windows scales it down for the title bar).
const WINDOW_ICON_SIZE: u32 = 64;

fn main() {
  let args: Vec<String> = std::env::args().collect();
  let is_daemon = args.iter().any(|a| a == "--daemon");
  app_log::init(if is_daemon { "service" } else { "ui" });

  if is_daemon {
    run_background_daemon();
  } else {
    run_gui_mode();
  }
}

/// Headless background audio service (runs on Windows boot or detached)
fn run_background_daemon() {
  info!("[GamersEqualizer] Initializing headless background service (< 4 MB RAM)...");

  // Single-instance daemon guard via named Win32 mutex
  let mutex_name = w!(r"Local\GamersEqualizer_Daemon_Mutex");
  let mutex = unsafe { CreateMutexW(None, true, mutex_name) };
  if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
    info!("[GamersEqualizer] Background daemon is already active. Exiting duplicate process.");
    return;
  }
  debug!("[GamersEqualizer] Acquired daemon mutex.");

  unsafe {
    let res = CoInitializeEx(None, COINIT_MULTITHREADED);
    debug!("[GamersEqualizer] CoInitializeEx: {:?}", res);
  }

  let storage = ProfileStorage::default_storage();
  win_audio::app_channels::init(storage.base_dir());
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
  let (shm_handle, shm_ptr) = ui::state::open_or_create_shm(storage.base_dir());
  let mut last_mic_version = 0;
  let mut last_default_mic: Option<String> = None;
  let mut last_default_out: Option<String> = None;
  if let Some(ptr) = shm_ptr {
    let header = unsafe { &mut (*ptr.as_ptr()).header };
    header.daemon_active.store(true, Ordering::SeqCst);
    header.shutdown_daemon_signal.store(false, Ordering::SeqCst);

    let devices = win_audio.endpoint_lock.enumerate_playback_devices();
    let def_id = win_audio.endpoint_lock.get_default_output_id();
    win_audio.chatmix.bind_endpoints(&devices, def_id.as_deref());
    last_default_out = def_id;
    // Saved (or, if the window runs, current) channel levels instead of 100 %.
    ui::audio_settings::restore_mixer(header, &mut win_audio.chatmix);
    // The mic the user picked, if it is still connected.
    let captures = win_audio.endpoint_lock.enumerate_capture_devices();
    if let Some(id) = header.get_selected_mic_id().filter(|id| captures.iter().any(|d| &d.id == id)) {
      win_audio.chatmix.mic_device_id = Some(id);
    }
    last_mic_version = header.mic_id_version.load(Ordering::Relaxed);
    last_default_mic = win_audio.endpoint_lock.get_default_capture_id();

    header.master_volume_permille.store((win_audio.chatmix.master_volume * 1000.0).round() as u32, Ordering::Relaxed);
    header.master_muted.store(win_audio.chatmix.master_muted, Ordering::Relaxed);
    header.game_volume_permille.store((win_audio.chatmix.game_volume * 1000.0).round() as u32, Ordering::Relaxed);
    header.game_muted.store(win_audio.chatmix.game_muted, Ordering::Relaxed);
    header.chat_volume_permille.store((win_audio.chatmix.chat_volume * 1000.0).round() as u32, Ordering::Relaxed);
    header.chat_muted.store(win_audio.chatmix.chat_muted, Ordering::Relaxed);
    header.media_volume_permille.store((win_audio.chatmix.media_volume * 1000.0).round() as u32, Ordering::Relaxed);
    header.media_muted.store(win_audio.chatmix.media_muted, Ordering::Relaxed);
    header.mic_volume_permille.store((win_audio.chatmix.mic_volume * 1000.0).round() as u32, Ordering::Relaxed);
    header.mic_muted.store(win_audio.chatmix.mic_muted, Ordering::Relaxed);
  }

  let mut app_state = AppState::init(storage);
  app_state.apply_output_eq();

  // Start Native WASAPI Microphone Audio Bridge in background daemon
  let initial_mic_id =
    win_audio.chatmix.mic_device_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
  win_audio.mic_stream.set_input_device(initial_mic_id);
  // Echo reference = the physical output (Master), what actually reaches the speakers.
  win_audio.mic_stream.set_reference_device(win_audio.chatmix.master_device_id.clone());
  win_audio.mic_stream.start();

  let mut last_volume_sync: Option<Instant> = None;
  let mut last_output_check: Option<Instant> = None;
  // Windows mic volume as the leveler's input gain, stepped at the extremes only.
  let mut mic_gain = win_audio::mic_gain::MicGainLink::default();
  // Debug lines for every leveler change and its reason (written only with "save debug" on).
  let mut leveler_log = win_audio::leveler_log::LevelerLog::default();

  // Notification-area icon: the window "hides to tray" by closing, the service keeps the icon.
  let mut tray_host = tray::TrayHost::new();
  // Setup problems (default output / spatial sound lost, mic processing) for the tray badge.
  let mut health = health::HealthMonitor::new(app_state.storage.base_dir());

  info!("[GamersEqualizer] Background service running smoothly. Entering audio monitoring loop...");

  loop {
    match tray_host.poll() {
      Some(tray::TrayAction::Exit) => {
        info!("[GamersEqualizer] Exit chosen in the tray. Closing the window and the service.");
        // Close the window first, or its watchdog would start the service again.
        tray::close_window();
        if let Some(ptr) = shm_ptr {
          unsafe { ptr.as_ref() }.header.daemon_active.store(false, Ordering::SeqCst);
        }
        break;
      }
      Some(tray::TrayAction::Fix(fix)) => health.apply(&fix, &win_audio),
      None => {}
    }
    if let Some(issues) = health.tick(&win_audio) {
      tray_host.set_issues(issues);
    }

    // Check shutdown signal from UI or user
    if let Some(ptr) = shm_ptr {
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
      // A device came or went: the picked mic may have been unplugged (the default does not
      // change when it was not the default) or plugged back in.
      let devices_changed = win_audio.endpoint_lock.poll_device_changed();
      if cur_mic_ver != last_mic_version || default_mic != last_default_mic || devices_changed {
        last_mic_version = cur_mic_ver;
        last_default_mic = default_mic;
        let captures = win_audio.endpoint_lock.enumerate_capture_devices();
        let picked = header.get_selected_mic_id().filter(|id| captures.iter().any(|d| &d.id == id));
        let wanted = picked.or_else(win_audio::ChatMixController::find_physical_microphone);
        if wanted.is_some() && wanted != win_audio.chatmix.mic_device_id {
          info!("[GamersEqualizer] microphone now {:?}", wanted);
          let old = win_audio.chatmix.mic_device_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
          mic_gain.switch_mic(&mut win_audio.chatmix, old.as_deref());
          win_audio.chatmix.mic_device_id = wanted.clone();
          win_audio.mic_stream.set_input_device(wanted);
        }
      }

      // The Windows default output changed (in Windows, in the window, in another app): the
      // Master channel and the echo reference follow it. Only the output is re-resolved; the
      // mic choice and levels stay (`bind_endpoints` would reset them). Before this the service
      // kept capturing the reference from the output it started with.
      if last_output_check.is_none_or(|t| t.elapsed() >= VOLUME_SYNC_INTERVAL) {
        last_output_check = Some(Instant::now());
        let default_out = win_audio.endpoint_lock.get_default_output_id();
        if default_out.is_some() && default_out != last_default_out {
          let devices = win_audio.endpoint_lock.enumerate_playback_devices();
          let master = win_audio::chatmix::physical_endpoint_id(&devices, default_out.as_deref());
          if master != win_audio.chatmix.master_device_id {
            info!("[GamersEqualizer] default output changed: Master and echo reference now {:?}", master);
            win_audio.chatmix.master_device_id = master;
          }
          last_default_out = default_out;
        }
      }
      // Follow output device changes for the echo reference (reopens only on change).
      win_audio.mic_stream.set_reference_device(win_audio.chatmix.master_device_id.clone());

      // 1. Sync live Windows master & channel volumes to daemon & SHM
      let sync_due = last_volume_sync.is_none_or(|t| t.elapsed() >= VOLUME_SYNC_INTERVAL);
      if sync_due {
        last_volume_sync = Some(Instant::now());
        // The wish (may be a boost > 100 %) is what the window wrote; Windows only shows <= 100 %.
        win_audio.chatmix.mic_volume = header.mic_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      }
      if sync_due && win_audio.chatmix.sync_from_system() {
        header
          .master_volume_permille
          .store((win_audio.chatmix.master_volume * 1000.0).round() as u32, Ordering::Relaxed);
        header.master_muted.store(win_audio.chatmix.master_muted, Ordering::Relaxed);
        header.game_volume_permille.store((win_audio.chatmix.game_volume * 1000.0).round() as u32, Ordering::Relaxed);
        header.game_muted.store(win_audio.chatmix.game_muted, Ordering::Relaxed);
        header.chat_volume_permille.store((win_audio.chatmix.chat_volume * 1000.0).round() as u32, Ordering::Relaxed);
        header.chat_muted.store(win_audio.chatmix.chat_muted, Ordering::Relaxed);
        header.media_volume_permille.store((win_audio.chatmix.media_volume * 1000.0).round() as u32, Ordering::Relaxed);
        header.media_muted.store(win_audio.chatmix.media_muted, Ordering::Relaxed);
        header.mic_volume_permille.store((win_audio.chatmix.mic_volume * 1000.0).round() as u32, Ordering::Relaxed);
        header.mic_muted.store(win_audio.chatmix.mic_muted, Ordering::Relaxed);
      }
      if sync_due {
        let mic_id = win_audio.chatmix.mic_device_id.clone().or_else(|| win_audio.endpoint_lock.get_default_capture_id());
        if mic_gain.tick(header, &mut win_audio.chatmix, mic_id.as_deref()) {
          app_state.save_audio_settings_if_changed(false);
        }
        leveler_log.tick(header);
      }

      // 2. Read sub-bus levels from SHM for background session scaling
      let g_vol = header.game_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      let c_vol = header.chat_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      let med_vol = header.media_volume_permille.load(Ordering::Relaxed) as f32 / 1000.0;

      let g_mute = header.game_muted.load(Ordering::Relaxed);
      let c_mute = header.chat_muted.load(Ordering::Relaxed);
      let med_mute = header.media_muted.load(Ordering::Relaxed);

      let bal = (header.chatmix_balance_permille.load(Ordering::Relaxed) as f32) / 1000.0;

      win_audio.chatmix.game_volume = g_vol;
      win_audio.chatmix.chat_volume = c_vol;
      win_audio.chatmix.media_volume = med_vol;

      win_audio.chatmix.game_muted = g_mute;
      win_audio.chatmix.chat_muted = c_mute;
      win_audio.chatmix.media_muted = med_mute;

      win_audio.chatmix.balance = bal;

      // Per-app session volumes: fallback mode only, throttled, never against the user's
      // own mixer changes (`apply_session_mix`).
      win_audio.chatmix.apply_session_mix();
    }

    // Game detection: the daemon is the owner (decides and publishes the active game).
    if let Some(ref detector) = win_audio.game_detector {
      while let Some(event) = detector.poll_event() {
        win_audio.chatmix.set_current_game(Some(event.exe_name.clone()));
        app_state.output_eq_do(|eq, shm| eq.on_foreground(&event, true, shm));
      }
    }
    let output = win_audio.chatmix.master_device_id.clone();
    app_state.output_eq_do(|eq, shm| eq.tick(true, output, shm));

    std::thread::sleep(Duration::from_millis(50));
  }

  if let Some(ptr) = shm_ptr {
    unsafe {
      let _ =
        UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.as_ptr() as *mut _ });
    }
  }
  if let Some(h) = shm_handle {
    unsafe {
      let _ = CloseHandle(h);
    }
  }
  if let Ok(m) = mutex {
    unsafe {
      let _ = CloseHandle(m);
    }
  }
}

/// Standalone UI Control Center (can be opened, closed, and reopened at will)
fn run_gui_mode() {
  info!("[GamersEqualizer] Starting UI Control Center...");

  // One window only: a second launch (tray, Start menu) brings the open one to the front.
  if tray::focus_window() {
    info!("[GamersEqualizer] Window already open; brought it to the front.");
    return;
  }

  // Ensure background service is running; if not, spawn it detached
  ensure_daemon_running();

  unsafe {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
  }

  let storage = ProfileStorage::default_storage();
  win_audio::app_channels::init(storage.base_dir());

  // Import once: also when the profiles predate launcher ids (needed for game detection),
  // which is the one time it overwrites same-named profiles. Then never again.
  let default_db = Path::new(DEFAULT_SONAR_DB_PATH);
  let eq_settings_path = storage.base_dir().join(output_eq::settings::SETTINGS_FILE);
  let mut eq_settings = output_eq::settings::OutputEqSettings::load_or_default(&eq_settings_path);
  if !eq_settings.imported_sonar && default_db.exists() {
    let games = storage.load_profiles_for_vad(gamersequalizer_common::profile::VadType::Game);
    let done = if games.iter().all(|p| p.game_ids.is_empty()) {
      import_sonar_db(default_db, &storage).map_err(|e| log::warn!("[Sonar] import failed: {e}")).is_ok()
    } else {
      true // imported by an older version already
    };
    if done {
      eq_settings.imported_sonar = true;
      if let Err(e) = eq_settings.save(&eq_settings_path) {
        log::warn!("[Sonar] could not save {}: {e}", eq_settings_path.display());
      }
    }
  }

  let mut win_audio = match WinAudioSystem::new() {
    Ok(w) => w,
    Err(e) => {
      error!("[GamersEqualizer] Failed to initialize Windows Audio: {}", e);
      return;
    }
  };

  let mut state = AppState::init(storage);
  state.apply_output_eq();

  // Bind channel endpoints and synchronize live volumes
  let devices = win_audio.endpoint_lock.enumerate_playback_devices();
  let def_id = win_audio.endpoint_lock.get_default_output_id();
  win_audio.chatmix.bind_endpoints(&devices, def_id.as_deref());
  // The service's (or the saved) channel levels; the window would otherwise reset them to 100 %.
  if let Some(h) = state.shm_header() {
    ui::audio_settings::restore_mixer(h, &mut win_audio.chatmix);
  }
  if let Some(id) = state.selected_mic_endpoint_id.clone() {
    win_audio.chatmix.mic_device_id = Some(id);
  }

  if let Some(ref mid) = win_audio.chatmix.master_device_id {
    state.selected_endpoint_id = Some(mid.clone());
  }
  if let Some(ref cid) = win_audio.chatmix.chat_device_id {
    state.selected_chat_endpoint_id = Some(cid.clone());
  }

  // Update SHM with live channel volumes
  state.sync_mixer_to_shm(&win_audio.chatmix);

  // The mic bridge, the echo reference loopback and the APO link run only in the background
  // service (started above, restarted by the window's watchdog). The window used to start its
  // own when the service was not up within 150 ms of a cold start, and never stopped it: two
  // bridges on one virtual mic, two APO owners mixing echo reference frames.

  // Last size/position/maximized state; the default fits a 1366x768 laptop and everything
  // scrolls / reflows when the window is made smaller.
  let geometry = ui::window_state::WindowGeometry::load(state.storage.base_dir());
  let window = ui::window_state::WindowTracker::new(state.storage.base_dir(), geometry);
  let mut viewport = egui::ViewportBuilder::default()
    .with_title(tray::WINDOW_TITLE)
    .with_icon(std::sync::Arc::new(egui::IconData {
      rgba: app_icon::rgba(WINDOW_ICON_SIZE, false, false),
      width: WINDOW_ICON_SIZE,
      height: WINDOW_ICON_SIZE,
    }))
    .with_inner_size(ui::window_state::DEFAULT_SIZE)
    .with_min_inner_size(ui::window_state::MIN_SIZE)
    .with_active(true);
  if let Some(g) = geometry {
    viewport = viewport.with_inner_size([g.width, g.height]).with_maximized(g.maximized);
    if g.has_position() {
      viewport = viewport.with_position([g.x, g.y]);
    }
  }
  let mut wgpu_setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
  wgpu_setup.instance_descriptor.backends = wgpu_backends();
  let wgpu_options =
    egui_wgpu::WgpuConfiguration { wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(wgpu_setup), ..Default::default() };
  let native_options = eframe::NativeOptions { viewport, wgpu_options, ..Default::default() };

  let app = GamersEqualizerApp::new(state, win_audio, window);

  let _ = eframe::run_native(
    "GamersEqualizer",
    native_options,
    Box::new(|cc| {
      ui::theme::install(&cc.egui_ctx);
      cc.egui_ctx.set_theme(app.state.prefs.egui_theme());
      Ok(Box::new(app))
    }),
  );

  info!("[GamersEqualizer] UI Control Center closed. Background service continues running uninterrupted.");
}

/// Check if daemon mutex exists, and start detached background process if not.
/// Also called periodically by the UI: the mutex disappears with the daemon process (crash,
/// kill), unlike the `daemon_active` flag in shared memory, which a killed daemon leaves set.
pub(crate) fn ensure_daemon_running() {
  let mutex_name = w!(r"Local\GamersEqualizer_Daemon_Mutex");
  let mutex = unsafe { CreateMutexW(None, false, mutex_name) };
  let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
  if let Ok(m) = mutex {
    unsafe {
      let _ = CloseHandle(m);
    }
  }

  if !already_running {
    if let Ok(exe_path) = std::env::current_exe() {
      info!("[GamersEqualizer] Spawning detached background daemon: {:?}", exe_path);
      let _ = Command::new(&exe_path).arg("--daemon").creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn();
      std::thread::sleep(Duration::from_millis(150));
    }
  }
}

/// Graphics backend for the window: DX12 when the machine has a DX12 GPU, else wgpu's usual
/// Vulkan/GL. Not Vulkan first: on some drivers wgpu 22 got "Suboptimal present" every frame,
/// rebuilds the swapchain each time and floods the log. Not DX12 only: wgpu picks one backend
/// up front and the window can't be recreated if it fails, so DX12 is used only when present.
/// `WGPU_BACKEND=vulkan|dx12|gl` still overrides the choice.
fn wgpu_backends() -> wgpu::Backends {
  if let Some(from_env) = wgpu::Backends::from_env() {
    return from_env;
  }
  let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
  desc.backends = wgpu::Backends::DX12;
  let probe = wgpu::Instance::new(desc);
  // Native enumeration finishes inside the first poll (the future exists for WebGPU), so one
  // poll with a no-op waker is enough; a pending result counts as "no DX12 GPU".
  let mut adapters = std::pin::pin!(probe.enumerate_adapters(wgpu::Backends::DX12));
  let adapters = match std::future::Future::poll(adapters.as_mut(), &mut std::task::Context::from_waker(std::task::Waker::noop())) {
    std::task::Poll::Ready(list) => list,
    std::task::Poll::Pending => Vec::new(),
  };
  let has_dx12_gpu = adapters.iter().any(|a| a.get_info().device_type != wgpu::DeviceType::Cpu);
  if has_dx12_gpu {
    info!("[GamersEqualizer] graphics: DX12");
    wgpu::Backends::DX12
  } else {
    info!("[GamersEqualizer] graphics: no DX12 GPU, using Vulkan/GL");
    wgpu::Backends::PRIMARY | wgpu::Backends::GL
  }
}
