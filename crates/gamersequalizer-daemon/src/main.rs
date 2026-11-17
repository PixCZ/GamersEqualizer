#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod apo_notes;
pub mod app_icon;
pub mod app_log;
pub mod autostart;
pub mod health;
pub mod i18n;
pub mod i18n_cs;
pub mod mic_usage;
pub mod notify;
pub mod output_eq;
pub mod settings_transfer;
pub mod setup_cli;
pub mod shm_mapping;
pub mod sonar_importer;
pub mod takeover;
pub mod tray;
pub mod ui;
pub mod uninstall;
pub mod win_audio;
pub mod win_clipboard;

use eframe::egui_wgpu::{self, wgpu};
use gamersequalizer_common::shm::ShmLayout;
use log::{debug, error, info};
use sonar_importer::ProfileStorage;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ui::{AppState, GamersEqualizerApp};
use win_audio::WinAudioSystem;
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Memory::UnmapViewOfFile;
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::core::w;

mod daemon;
mod lifecycle;

use daemon::*;
use lifecycle::*;

/// Passed to the installed copy by `hand_over_to_installed`: it must not hand over again.
const NO_HANDOVER_ARG: &str = "--no-handover";
/// How long the window waits for its service to exit before starting the installed app.
const SERVICE_EXIT_WAIT: Duration = Duration::from_secs(5);
/// Set by the window once the installer has put the app into Program Files: when the window
/// has closed, `run_gui_mode` starts the installed app.
static MOVE_TO_INSTALLED: AtomicBool = AtomicBool::new(false);
/// The window asked the service to stop before it closed ("Stop service and exit", uninstall):
/// the closing log line must not say the service runs on.
static SERVICE_STOP_ASKED: AtomicBool = AtomicBool::new(false);

/// A settings file to import once the window has closed and the service has ended (nothing
/// may write settings meanwhile); then the app starts again (`import_after_close`).
static PENDING_IMPORT: Mutex<Option<(PathBuf, settings_transfer::Selection)>> = Mutex::new(None);
/// Passed to the window started after an import: "ok" or the error, shown in Settings.
const IMPORT_RESULT_ARG: &str = "--import-result";

/// Window / taskbar icon resolution (Windows scales it down for the title bar).
const WINDOW_ICON_SIZE: u32 = 64;

fn main() {
  let args: Vec<String> = std::env::args().collect();
  // The installer (elevated) opens endpoints briefly to see whether Windows starts our effects.
  if let Some(guid) = args.iter().position(|a| a == "--probe-mic").and_then(|i| args.get(i + 1)) {
    app_log::init("probe");
    std::process::exit(win_audio::mic_probe::run(guid));
  }
  if let Some(guid) = args.iter().position(|a| a == "--probe-render").and_then(|i| args.get(i + 1)) {
    app_log::init("probe");
    std::process::exit(win_audio::mic_probe::run_render(guid));
  }
  let is_daemon = args.iter().any(|a| a == "--daemon");
  app_log::init(if is_daemon { "service" } else { "ui" });
  // Before the handover: `--install` from the ZIP must install this copy, not start the old one.
  if let Some(code) = setup_cli::run(&args) {
    std::process::exit(code);
  }
  // Once installed, the app lives next to its effects in Program Files: a copy started from
  // elsewhere (the unzipped folder, an old autostart entry) starts that one instead.
  if !args.iter().any(|a| a == NO_HANDOVER_ARG) && hand_over_to_installed(args.get(1..).unwrap_or_default()) {
    return;
  }

  if is_daemon {
    run_background_daemon();
  } else {
    run_gui_mode();
  }
}

/// Standalone UI Control Center (can be opened, closed, and reopened at will)
fn run_gui_mode() {
  info!("[GamersEqualizer] Starting UI Control Center...");

  // A new version was unpacked and started: it ends the older one that is running, then
  // offers to update the installation.
  takeover::replace_older_copies();

  // One window only: a second launch (tray, Start menu) brings the open one to the front.
  if tray::focus_window() {
    info!("[GamersEqualizer] Window already open; brought it to the front.");
    return;
  }

  // Ensure background service is running; if not, spawn it detached
  ensure_daemon_running();

  // SAFETY: COM init for the window thread with no reserved pointer; it stays for the window's life.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
  }

  let storage = ProfileStorage::default_storage();
  win_audio::app_channels::init(storage.base_dir());
  win_audio::sidetone::init(storage.base_dir());

  // The Sonar profiles are offered on the first-start setup card (`ui::setup`), not imported here.

  let mut win_audio = match WinAudioSystem::new() {
    Ok(w) => w,
    Err(e) => {
      error!("[GamersEqualizer] Failed to initialize Windows Audio: {}", e);
      return;
    }
  };

  let mut state = AppState::init(storage);
  let args: Vec<String> = std::env::args().collect();
  if let Some(result) = args.iter().position(|a| a == IMPORT_RESULT_ARG).and_then(|i| args.get(i + 1)) {
    state.transfer_ui.show_import_result(result);
  }
  state.apply_output_eq();

  // Bind channel endpoints and synchronize live volumes
  let devices = win_audio.endpoint_lock.enumerate_playback_devices();
  let def_id = win_audio.endpoint_lock.get_default_output_id();
  win_audio.chatmix.bind_endpoints(&devices, def_id.as_deref());
  // The saved pick only while it is connected (like the service and the window's re-bind).
  let captures = win_audio.endpoint_lock.enumerate_capture_devices();
  win_audio.chatmix.use_mic(state.selected_mic_endpoint_id.clone().filter(|id| captures.iter().any(|d| &d.id == id)));
  // The service's (or the saved) channel levels; the window would otherwise reset them to 100 %.
  // Last, so the running service's mic wish wins over what this process read.
  if let Some(h) = state.shm_header() {
    ui::audio_settings::restore_mixer(h, &mut win_audio.chatmix, h.daemon_active.load(Ordering::Relaxed));
  }

  if let Some(ref mid) = win_audio.chatmix.master_device_id {
    state.selected_endpoint_id = Some(mid.clone());
  }
  if let Some(ref cid) = win_audio.chatmix.chat_device_id {
    state.selected_chat_endpoint_id = Some(cid.clone());
  }

  // Update SHM with live channel volumes
  state.sync_mixer_to_shm(&win_audio.chatmix);

  // The echo reference loopback and the APO link run only in the background service (started
  // above, restarted by the window's watchdog). The window used to start its own when the
  // service was not up within 150 ms of a cold start, and never stopped it: two APO owners
  // mixing echo reference frames.

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
  wgpu_setup.power_preference = wgpu::PowerPreference::LowPower;
  let wgpu_options =
    egui_wgpu::WgpuConfiguration { wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(wgpu_setup), ..Default::default() };
  let native_options = eframe::NativeOptions { viewport, wgpu_options, ..Default::default() };

  let app = GamersEqualizerApp::new(state, win_audio, window);

  let _ = eframe::run_native(
    "GamersEqualizer",
    native_options,
    Box::new(|cc| {
      log_graphics_adapters(cc.wgpu_render_state.as_ref());
      ui::theme::install(&cc.egui_ctx);
      cc.egui_ctx.set_theme(app.state.prefs.egui_theme());
      Ok(Box::new(app))
    }),
  );

  let pending_import = PENDING_IMPORT.lock().unwrap_or_else(|e| e.into_inner()).take();
  if let Some((path, only)) = pending_import {
    let asked = Instant::now();
    while daemon_running() && asked.elapsed() < SERVICE_EXIT_WAIT {
      std::thread::sleep(Duration::from_millis(100));
    }
    if daemon_running() {
      log::warn!("[Transfer] the service has not exited yet; importing anyway");
    }
    let result = match settings_transfer::import(ProfileStorage::default_storage().base_dir(), &path, Some(&only)) {
      Ok(_) => "ok".to_string(),
      Err(e) => {
        error!("[Transfer] import of {} failed: {e}", path.display());
        e
      }
    };
    match std::env::current_exe() {
      Ok(exe) => {
        if let Err(e) = Command::new(exe).args([IMPORT_RESULT_ARG, &result]).spawn() {
          error!("[Transfer] could not start the app again: {e}");
        }
      }
      Err(e) => error!("[Transfer] could not locate the executable: {e}"),
    }
    return;
  }
  if MOVE_TO_INSTALLED.load(Ordering::Relaxed) {
    // Only now, with this window gone (the installed app would find it and just focus it). Its
    // service was asked to stop and holds the single-instance mutex until it has exited.
    let asked = Instant::now();
    while daemon_running() && asked.elapsed() < SERVICE_EXIT_WAIT {
      std::thread::sleep(Duration::from_millis(100));
    }
    if !hand_over_to_installed(&[]) {
      // The installed app did not start: at least the effects keep their settings.
      ensure_daemon_running();
    }
    return;
  }
  if SERVICE_STOP_ASKED.load(Ordering::Relaxed) {
    info!("[GamersEqualizer] UI Control Center closed. The background service was asked to stop.");
  } else {
    info!("[GamersEqualizer] UI Control Center closed. Background service continues running uninterrupted.");
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
  let adapters =
    match std::future::Future::poll(adapters.as_mut(), &mut std::task::Context::from_waker(std::task::Waker::noop())) {
      std::task::Poll::Ready(list) => list,
      std::task::Poll::Pending => Vec::new(),
    };
  let has_dx12_gpu = adapters.iter().any(|a| a.get_info().device_type != wgpu::DeviceType::Cpu);
  if has_dx12_gpu {
    // No adapter name: wgpu picks the adapter later (low-power preference), not this probe.
    info!("[GamersEqualizer] graphics: DX12 (low power preference)");
    wgpu::Backends::DX12
  } else {
    info!("[GamersEqualizer] graphics: no DX12 GPU, using Vulkan/GL");
    wgpu::Backends::PRIMARY | wgpu::Backends::GL
  }
}

/// Logs the GPU the window draws on and every adapter wgpu listed. On DX12 each listed adapter
/// holds an open D3D12 device and egui-wgpu keeps the list while the window lives, so a vendor
/// tool can show the app on a GPU it never draws on.
fn log_graphics_adapters(render_state: Option<&egui_wgpu::RenderState>) {
  let Some(render_state) = render_state else {
    return;
  };
  let describe = |info: wgpu::AdapterInfo| format!("{} ({:?}, {:?})", info.name, info.device_type, info.backend);
  let listed: Vec<String> = render_state.available_adapters.iter().map(|a| describe(a.get_info())).collect();
  info!(
    "[GamersEqualizer] graphics: drawing on {}; adapters listed: {}",
    describe(render_state.adapter.get_info()),
    listed.join(", ")
  );
}
