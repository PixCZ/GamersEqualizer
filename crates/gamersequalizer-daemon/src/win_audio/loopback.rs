//! Speaker reference for the echo canceller: WASAPI loopback capture of the output device,
//! written with QPC timestamps into the shared-memory ring.

use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
  AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
  AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
  WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
  AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};

use gamersequalizer_common::shm::StereoFrame;
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;

use super::apo_link::ApoLink;

const LOOPBACK_BUFFER_MS: i64 = 20;
const HNS_PER_MS: i64 = 10_000;
/// How often an unpinned stream checks whether the Windows default output changed.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// AUDCLNT_E_DEVICE_INVALIDATED as it appears in the error text: expected after an audio
/// service restart (APO install) or a device change; the loop simply reopens.
const DEVICE_INVALIDATED: &str = "0x88890004";
/// Idle wait between APO mirror ticks. Known limit: when an app opens the mic, the echo
/// canceller gets its reference ~100-200 ms later (first telemetry, then the stream opens).
const IDLE_POLL: Duration = Duration::from_millis(50);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
  windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// The echo canceller's reference runs at 48 kHz; WASAPI resamples to it.
const DSP_SAMPLE_RATE: u32 = 48_000;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;

/// Format a client was actually initialized with.
struct StreamFormat {
  channels: usize,
  sample_rate: u32,
  is_float: bool,
}

/// Owns the raw handles/allocations of one stream session so every exit path frees them.
struct StreamResources {
  cap_event: HANDLE,
  cap_fmt: *mut WAVEFORMATEX,
  mmcss: Option<HANDLE>,
}

impl StreamResources {
  fn new() -> Self {
    Self { cap_event: HANDLE::default(), cap_fmt: std::ptr::null_mut(), mmcss: None }
  }

  /// Raise this thread to an MMCSS task class; failure only costs glitch resistance.
  fn register_mmcss(&mut self, task: PCWSTR) {
    let mut task_index = 0u32;
    match unsafe { AvSetMmThreadCharacteristicsW(task, &mut task_index) } {
      Ok(h) => self.mmcss = Some(h),
      Err(e) => warn!("[Loopback] MMCSS registration failed (continuing): {}", e),
    }
  }
}

impl Drop for StreamResources {
  fn drop(&mut self) {
    unsafe {
      if let Some(h) = self.mmcss.take() {
        let _ = AvRevertMmThreadCharacteristics(h);
      }
      if !self.cap_event.is_invalid() {
        let _ = CloseHandle(self.cap_event);
      }
      if !self.cap_fmt.is_null() {
        CoTaskMemFree(Some(self.cap_fmt as *const _));
      }
    }
  }
}

/// Spawn the supervised loopback thread. Runs until `running` is cleared.
pub fn spawn(
  running: Arc<AtomicBool>,
  reference_id: Arc<Mutex<Option<String>>>,
) -> Option<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name("GamersEqualizer_SpeakerLoopback".into())
    .spawn(move || {
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      let mut shm = match SharedMemoryClient::open_or_create() {
        Ok(c) => c,
        Err(e) => {
          warn!("[Loopback] shared memory unavailable, AEC will have no reference: {}", e);
          return;
        }
      };
      let mut apo = ApoLink::new();
      info!("[Loopback] speaker reference thread started");
      while running.load(Ordering::SeqCst) {
        // No app records through the mic filter: no reference capture, only the APO mirroring
        // (settings in, telemetry out), which also tells when the filter starts again.
        if !apo.mic_dsp_live() {
          apo.tick(&shm, false);
          std::thread::sleep(IDLE_POLL);
          continue;
        }
        let wanted = reference_id.lock().ok().and_then(|g| g.clone());
        if let Err(e) = run_loopback(&running, &reference_id, wanted.as_deref(), &mut shm, &mut apo) {
          if e.contains(DEVICE_INVALIDATED) {
            info!("[Loopback] output device was reset (Windows audio restarted or device changed), reconnecting");
          } else {
            warn!("[Loopback] iteration error: {}. Retrying in 1 s...", e);
          }
          apo.tick(&shm, false);
          std::thread::sleep(Duration::from_secs(1));
        }
      }
      shm.layout().header.tap_active.store(false, Ordering::Relaxed);
      apo.set_tap_active(false);
      info!("[Loopback] speaker reference thread stopped");
    })
    .ok()
}

/// Id of the render device the reference is taken from: the explicit one, else the
/// current Windows default output.
pub fn resolve_render_id(explicit: Option<&str>) -> Option<String> {
  if let Some(id) = explicit {
    return Some(id.to_string());
  }
  unsafe {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
    device_id(&dev)
  }
}

/// Id of the Windows default microphone (the endpoint the APO most likely runs on).
pub fn resolve_default_capture_id() -> Option<String> {
  unsafe {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let dev = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;
    device_id(&dev)
  }
}

unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
  let pw = dev.GetId().ok()?;
  let id = pw.to_string().ok();
  CoTaskMemFree(Some(pw.as_ptr() as *const _));
  id
}

fn run_loopback(
  running: &Arc<AtomicBool>,
  wanted: &Mutex<Option<String>>,
  explicit_id: Option<&str>,
  shm: &mut SharedMemoryClient,
  apo: &mut ApoLink,
) -> Result<(), String> {
  unsafe {
    let mut res = StreamResources::new();
    res.register_mmcss(w!("Audio"));

    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
      .map_err(|e| format!("CoCreateInstance MMDeviceEnumerator failed: {}", e))?;
    let dev: IMMDevice = match explicit_id {
      Some(id) => {
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| format!("GetDevice {}: {}", id, e))?
      }
      None => {
        enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("No default output device: {}", e))?
      }
    };
    let opened_id = device_id(&dev);

    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate: {}", e))?;
    let mix: *mut WAVEFORMATEX = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {}", e))?;
    res.cap_fmt = mix;
    res.cap_event = CreateEventW(None, false, false, None).map_err(|e| format!("CreateEventW: {}", e))?;

    let fmt = init_client_48k_float(
      &client,
      mix,
      AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
      LOOPBACK_BUFFER_MS * HNS_PER_MS,
    )?;
    if fmt.sample_rate != 48_000 || !fmt.is_float {
      return Err(format!(
        "loopback runs at {} Hz float={} (48 kHz float required for the reference)",
        fmt.sample_rate, fmt.is_float
      ));
    }
    client.SetEventHandle(res.cap_event).map_err(|e| format!("SetEventHandle: {}", e))?;
    let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("GetService: {}", e))?;
    client.Start().map_err(|e| format!("Start: {}", e))?;

    info!("[Loopback] capturing speaker reference from {:?} ({} ch)", opened_id, fmt.channels);
    apo.set_render_device(opened_id.clone());
    shm.layout().header.tap_active.store(true, Ordering::Relaxed);

    let mut frames_buf: Vec<StereoFrame> = Vec::with_capacity(2048);
    let mut last_default_check = Instant::now();

    while running.load(Ordering::SeqCst) {
      // Follow the user's device choice / Windows default output.
      let wanted_now = wanted.lock().ok().and_then(|g| g.clone());
      if wanted_now.as_deref() != explicit_id {
        info!("[Loopback] reference device selection changed, reopening");
        break;
      }
      if explicit_id.is_none() && last_default_check.elapsed() >= DEFAULT_CHECK_INTERVAL {
        last_default_check = Instant::now();
        // Reuses this stream's enumerator (a new one each second was needless COM churn).
        let default_now = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok().and_then(|d| device_id(&d));
        if default_now != opened_id {
          info!("[Loopback] Windows default output changed, reopening");
          break;
        }
      }

      // Event callback on loopback needs Win10+; the timeout keeps draining otherwise.
      let _ = WaitForSingleObject(res.cap_event, 10);

      loop {
        let mut p_data: *mut u8 = std::ptr::null_mut();
        let mut n: u32 = 0;
        let mut flags: u32 = 0;
        let mut qpc: u64 = 0;
        // An error (device invalidated, audio service restarted, e.g. by the APO installer)
        // must reopen the stream; treating it like "nothing to read" left a dead stream forever.
        capture
          .GetBuffer(&mut p_data, &mut n, &mut flags, None, Some(&mut qpc))
          .map_err(|e| format!("GetBuffer: {}", e))?;
        if n == 0 {
          break;
        }
        let frames = n as usize;
        frames_buf.clear();
        if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || p_data.is_null() {
          frames_buf.resize(frames, StereoFrame::default());
        } else {
          let ch = fmt.channels.max(1);
          let slice = std::slice::from_raw_parts(p_data as *const f32, frames * ch);
          frames_buf.extend(slice.chunks_exact(ch).map(StereoFrame::from_channels));
        }
        let _ = capture.ReleaseBuffer(n);
        if qpc != 0 {
          shm.ring_buffer().push_frames_timed(&frames_buf, qpc);
          apo.push_reference(&frames_buf, qpc);
        }
      }
      apo.tick(shm, true);
      if !apo.mic_dsp_live() {
        info!("[Loopback] microphone filter idle, speaker reference capture paused");
        break;
      }
    }

    let _ = client.Stop();
    shm.layout().header.tap_active.store(false, Ordering::Relaxed);
    apo.set_tap_active(false);
    Ok(())
  }
}

/// Initialize a shared-mode client at 48 kHz float32 (WASAPI converts and resamples);
/// falls back to the device mix format if the driver refuses.
unsafe fn init_client_48k_float(
  client: &IAudioClient,
  mix_fmt: *const WAVEFORMATEX,
  flags: u32,
  buffer_hns: i64,
) -> Result<StreamFormat, String> {
  let mix = *mix_fmt;
  // Plain WAVEFORMATEX only describes mono/stereo unambiguously; we only need channel 0 anyway.
  let channels = mix.nChannels.clamp(1, 2);
  let block_align = channels * 4;
  let wanted = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_IEEE_FLOAT_TAG,
    nChannels: channels,
    nSamplesPerSec: DSP_SAMPLE_RATE,
    nAvgBytesPerSec: DSP_SAMPLE_RATE * block_align as u32,
    nBlockAlign: block_align,
    wBitsPerSample: 32,
    cbSize: 0,
  };
  let convert = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
  match client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags | convert, buffer_hns, 0, &wanted, None) {
    Ok(()) => Ok(StreamFormat { channels: channels as usize, sample_rate: DSP_SAMPLE_RATE, is_float: true }),
    Err(e) => {
      warn!("[Loopback] 48 kHz float stream refused ({}), using device mix format", e);
      client
        .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_hns, 0, mix_fmt, None)
        .map_err(|e| format!("IAudioClient.Initialize failed: {}", e))?;
      Ok(StreamFormat {
        channels: (mix.nChannels as usize).max(1),
        sample_rate: mix.nSamplesPerSec,
        is_float: is_float_format(mix_fmt),
      })
    }
  }
}

unsafe fn is_float_format(p_fmt: *const WAVEFORMATEX) -> bool {
  if p_fmt.is_null() {
    return false;
  }
  let tag = (*p_fmt).wFormatTag as u32;
  if tag == 3 {
    return true; // WAVE_FORMAT_IEEE_FLOAT
  }
  if tag == 0xFFFE {
    // WAVE_FORMAT_EXTENSIBLE
    let ext = p_fmt as *const WAVEFORMATEXTENSIBLE;
    let sub_format = std::ptr::read_unaligned(std::ptr::addr_of!((*ext).SubFormat));
    return sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
  }
  false
}
