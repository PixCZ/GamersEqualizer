//! The thread behind the echo canceller's speaker reference. By default our output effect on the
//! reference output writes the ring itself (`aec_reference.rs`); this thread decides which output
//! that is and drives the link to the APOs. `GAMERSEQUALIZER_AEC_REF=loopback` instead captures the
//! output with WASAPI loopback and writes the ring from here, as before.

use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
  AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
  AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
  IAudioCaptureClient, IAudioClient, IMMDevice, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Threading::{
  AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};
use windows::core::{PCWSTR, w};

use gamersequalizer_common::shm::{AecRefStatus, StereoFrame};
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;

use super::aec_reference::OutputReference;
use super::apo_link::ApoLink;
use super::{mmdevice, worker};

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
/// Environment variable that picks the reference source (`loopback`); anything else = the APO.
const SOURCE_VARIABLE: &str = "GAMERSEQUALIZER_AEC_REF";

/// Where the echo canceller's speaker reference comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefSource {
  /// Our output effect on the reference output writes the ring (the default).
  OutputEffect,
  /// WASAPI loopback capture in this process.
  Loopback,
}

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
    // SAFETY: `task` is a static NUL-terminated name (`w!`) and the index a local; the handle is reverted in
    // `Drop`.
    match unsafe { AvSetMmThreadCharacteristicsW(task, &mut task_index) } {
      Ok(h) => self.mmcss = Some(h),
      Err(e) => warn!("[Loopback] MMCSS registration failed (continuing): {}", e),
    }
  }
}

impl Drop for StreamResources {
  fn drop(&mut self) {
    // SAFETY: the handles and the format block come from this session (`register_mmcss`, CreateEventW,
    // GetMixFormat), are checked for validity and released once, here; the COM objects that used them are locals
    // declared after this struct, so they are released first.
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
      // SAFETY: COM init for this thread with no reserved pointer; it stays for the thread's life.
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      let Some(mut shm) = worker::open_shm("Loopback", || running.load(Ordering::SeqCst)) else {
        return;
      };
      let source = RefSource::from_env();
      let mut seen = worker::RepeatFilter::default();
      let mut apo = ApoLink::new(source);
      info!(
        "[Loopback] speaker reference thread started, source {source:?} (set {SOURCE_VARIABLE}=loopback to compare)"
      );
      if source == RefSource::OutputEffect {
        let mut reference = OutputReference::default();
        while running.load(Ordering::SeqCst) {
          reference.tick(&shm, &mut apo, &reference_id);
          std::thread::sleep(IDLE_POLL);
        }
      } else {
        let h = &shm.layout().header;
        h.aec_ref_status.store(AecRefStatus::Loopback.to_u32(), Ordering::Relaxed);
        // The window keeps this mapping across a service restart: a hash left by a run with the
        // APO source would make an output APO write the ring beside the loopback.
        h.aec_ref_output_hash.store(0, Ordering::Relaxed);
      }
      while running.load(Ordering::SeqCst) {
        // No app records through the mic filter: no reference capture, only the APO mirroring
        // (settings in, telemetry out), which also tells when the filter starts again.
        if !apo.mic_dsp_live() {
          apo.tick(&shm, false);
          std::thread::sleep(IDLE_POLL);
          continue;
        }
        let wanted = reference_id.lock().ok().and_then(|g| g.clone());
        if let Err(e) = run_loopback(&running, &reference_id, wanted.as_deref(), &mut shm, &mut apo, &mut seen) {
          // An unplugged output fails the same way every second: logged once until it changes.
          if seen.is_new(&e) {
            if e.contains(DEVICE_INVALIDATED) {
              info!("[Loopback] output device was reset (Windows audio restarted or device changed), reconnecting");
            } else {
              warn!("[Loopback] iteration error: {}. Retrying every 1 s...", e);
            }
          }
          apo.tick(&shm, false);
          std::thread::sleep(Duration::from_secs(1));
        }
      }
      let h = &shm.layout().header;
      h.tap_active.store(false, Ordering::Relaxed);
      h.aec_ref_output_hash.store(0, Ordering::Relaxed);
      apo.set_tap_active(false);
      info!("[Loopback] speaker reference thread stopped");
    })
    .ok()
}

impl RefSource {
  /// Read once at service start.
  /// shortcut: the switch only keeps the loopback for the comparison on Windows; it goes away
  /// with the loopback once the measurement is in DEVLOG.
  fn from_env() -> Self {
    match std::env::var(SOURCE_VARIABLE) {
      Ok(v) if v.eq_ignore_ascii_case("loopback") => RefSource::Loopback,
      _ => RefSource::OutputEffect,
    }
  }
}

/// Id of the render device the reference is taken from: the explicit one, else the
/// current Windows default output.
pub fn resolve_render_id(explicit: Option<&str>) -> Option<String> {
  if let Some(id) = explicit {
    return Some(id.to_string());
  }
  // On a thread without COM this returns None.
  mmdevice::default_device_id(eRender, eConsole)
}

/// Id of the Windows default microphone (the endpoint the APO most likely runs on).
pub fn resolve_default_capture_id() -> Option<String> {
  mmdevice::default_device_id(eCapture, eConsole)
}

fn run_loopback(
  running: &Arc<AtomicBool>,
  wanted: &Mutex<Option<String>>,
  explicit_id: Option<&str>,
  shm: &mut SharedMemoryClient,
  apo: &mut ApoLink,
  seen: &mut worker::RepeatFilter,
) -> Result<(), String> {
  // SAFETY: COM calls on interfaces created in this block, on the loopback thread (COM initialized in `spawn`). The
  // device name is NUL-terminated and outlives the call; `res` owns the format block and the event and frees them
  // after the COM objects. GetBuffer's data holds `n` frames of `fmt.channels` floats (48 kHz float is checked
  // above) until ReleaseBuffer.
  unsafe {
    let mut res = StreamResources::new();
    res.register_mmcss(w!("Audio"));

    let enumerator =
      mmdevice::enumerator().map_err(|e| format!("CoCreateInstance MMDeviceEnumerator failed: {}", e))?;
    let dev: IMMDevice = match explicit_id {
      Some(id) => mmdevice::device(id).map_err(|e| format!("GetDevice {}: {}", id, e))?,
      None => {
        enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("No default output device: {}", e))?
      }
    };
    let opened_id = mmdevice::device_id(&dev);

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
    seen.reset();

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
        let default_now =
          enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok().and_then(|d| mmdevice::device_id(&d));
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
///
/// # Safety
/// `mix_fmt` is the client's own mix format (`GetMixFormat`), alive for the call; COM is initialized.
unsafe fn init_client_48k_float(
  client: &IAudioClient,
  mix_fmt: *const WAVEFORMATEX,
  flags: u32,
  buffer_hns: i64,
) -> Result<StreamFormat, String> {
  // SAFETY: the contract above; the wanted format is a local that outlives both Initialize calls.
  unsafe {
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
}

/// # Safety
/// `p_fmt` is null or a live `WAVEFORMATEX`, a whole `WAVEFORMATEXTENSIBLE` when its tag says so.
unsafe fn is_float_format(p_fmt: *const WAVEFORMATEX) -> bool {
  // SAFETY: the contract above: the tag is read first, the extensible fields only when the tag says they exist.
  unsafe {
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
}
