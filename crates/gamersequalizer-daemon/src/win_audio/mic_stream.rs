use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
  eCapture, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
  MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
  AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
  WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
  AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject,
};

use gamersequalizer_mic_vst::plugin::{GamersEqualizerPlugin, PluginMode};

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
  windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// The DSP chain (nnnoiseless, AEC) is tuned for 48 kHz; WASAPI resamples to/from it.
const DSP_SAMPLE_RATE: u32 = 48_000;
/// Bridge can't start without a virtual mic sink; expected when the APO path is used.
const NO_VIRTUAL_MIC: &str = "No virtual microphone render endpoint found (SteelSeries Sonar Mic or VB-Cable Input)";
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const HNS_PER_MS: i64 = 10_000;
const CAPTURE_BUFFER_MS: i64 = 10;
/// Render ring is larger than the cushion so mic/sink clock drift doesn't overflow it at once.
const RENDER_BUFFER_MS: i64 = 60;
/// Latency cushion kept queued in the virtual-mic sink.
const RENDER_TARGET_MS: u32 = 20;

const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// A stream that ran this long before failing was healthy: retry fast again (resume from sleep,
/// replug), not with the backoff left over from boot.
const HEALTHY_RUN: Duration = Duration::from_secs(10);

/// Check for a newly confirmed echo delay to persist this often (blocks of ~10 ms).
const DELAY_SAVE_CHECK_BLOCKS: u32 = 500;
/// Persist only real changes (> 1 ms).
const DELAY_SAVE_MIN_CHANGE: usize = 48;

pub struct MicStreamWorker {
  running: Arc<AtomicBool>,
  input_device_id: Arc<Mutex<Option<String>>>,
  output_device_id: Arc<Mutex<Option<String>>>,
  /// Speaker output used as echo reference; None = Windows default output.
  reference_device_id: Arc<Mutex<Option<String>>>,
  thread_handle: Option<JoinHandle<()>>,
  loopback_handle: Option<JoinHandle<()>>,
}

/// Format a client was actually initialized with.
pub(crate) struct StreamFormat {
  pub channels: usize,
  pub sample_rate: u32,
  pub bits: u16,
  pub is_float: bool,
}

/// Device ids the UI currently wants; the stream reopens when they differ from the open ones.
struct DeviceSelection<'a> {
  input: &'a Mutex<Option<String>>,
  output: &'a Mutex<Option<String>>,
  reference: Option<&'a Mutex<Option<String>>>,
}

/// Owns the raw handles/allocations of one stream session so every exit path frees them.
pub(crate) struct StreamResources {
  pub cap_event: HANDLE,
  pub cap_fmt: *mut WAVEFORMATEX,
  pub rnd_fmt: *mut WAVEFORMATEX,
  mmcss: Option<HANDLE>,
}

impl Default for MicStreamWorker {
  fn default() -> Self {
    Self::new()
  }
}

impl MicStreamWorker {
  pub fn new() -> Self {
    Self {
      running: Arc::new(AtomicBool::new(false)),
      input_device_id: Arc::new(Mutex::new(None)),
      output_device_id: Arc::new(Mutex::new(None)),
      reference_device_id: Arc::new(Mutex::new(None)),
      thread_handle: None,
      loopback_handle: None,
    }
  }

  /// Speaker device whose output is cancelled from the mic (None = Windows default).
  pub fn set_reference_device(&self, id: Option<String>) {
    if let Ok(mut lock) = self.reference_device_id.lock() {
      *lock = id;
    }
  }

  pub fn set_input_device(&self, id: Option<String>) {
    if let Ok(mut lock) = self.input_device_id.lock() {
      *lock = id;
    }
  }

  pub fn set_output_device(&self, id: Option<String>) {
    if let Ok(mut lock) = self.output_device_id.lock() {
      *lock = id;
    }
  }

  pub fn start(&mut self) {
    if self.running.load(Ordering::SeqCst) {
      return;
    }

    self.running.store(true, Ordering::SeqCst);
    let running = self.running.clone();
    let in_id_lock = self.input_device_id.clone();
    let out_id_lock = self.output_device_id.clone();
    let ref_id_lock = self.reference_device_id.clone();
    self.loopback_handle = super::loopback::spawn(running.clone(), self.reference_device_id.clone());

    let handle = std::thread::Builder::new()
      .name("GamersEqualizer_MicBridge".into())
      .spawn(move || {
        unsafe {
          let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        info!("[MicStream] Native WASAPI microphone bridge thread started.");

        let mut last_error: Option<String> = None;
        let mut backoff = RETRY_MIN;
        while running.load(Ordering::SeqCst) {
          let cur_in_id = in_id_lock.lock().ok().and_then(|g| g.clone());
          let cur_out_id = out_id_lock.lock().ok().and_then(|g| g.clone());
          let cur_ref_id = ref_id_lock.lock().ok().and_then(|g| g.clone());
          let wanted = DeviceSelection { input: &in_id_lock, output: &out_id_lock, reference: Some(&ref_id_lock) };

          let started = Instant::now();
          match run_bridge_stream(&running, &wanted, cur_in_id.as_deref(), cur_out_id.as_deref(), cur_ref_id.as_deref())
          {
            Ok(()) => {
              last_error = None;
              backoff = RETRY_MIN;
            }
            Err(e) => {
              if started.elapsed() >= HEALTHY_RUN {
                backoff = RETRY_MIN;
              }
              // Log each distinct failure once (e.g. "no virtual mic") instead of 2x per second.
              if last_error.as_deref() != Some(e.as_str()) {
                if e == NO_VIRTUAL_MIC {
                  // Normal with the microphone APO: it processes the real mic, no bridge needed.
                  info!("[MicStream] bridge idle: no virtual microphone device (Sonar Mic / VB-Cable). Not needed when the microphone APO is installed.");
                } else {
                  warn!("[MicStream] Bridge error: {}. Retrying with backoff up to {:?}.", e, RETRY_MAX);
                }
                last_error = Some(e);
              }
              sleep_while_running(&running, backoff);
              backoff = (backoff * 2).min(RETRY_MAX);
            }
          }
        }

        info!("[MicStream] Native WASAPI microphone bridge thread terminated cleanly.");
      })
      .ok();

    self.thread_handle = handle;
  }

  pub fn stop(&mut self) {
    self.running.store(false, Ordering::SeqCst);
    if let Some(h) = self.thread_handle.take() {
      let _ = h.join();
    }
    if let Some(h) = self.loopback_handle.take() {
      let _ = h.join();
    }
  }
}

impl Drop for MicStreamWorker {
  fn drop(&mut self) {
    self.stop();
  }
}

impl DeviceSelection<'_> {
  fn changed(&self, open_in: Option<&str>, open_out: Option<&str>) -> bool {
    let cur_in = self.input.lock().ok().and_then(|g| g.clone());
    let cur_out = self.output.lock().ok().and_then(|g| g.clone());
    cur_in.as_deref() != open_in || cur_out.as_deref() != open_out
  }

  /// The echo-delay cache key depends on the reference device, so reopen on change.
  fn reference_changed(&self, open_ref: Option<&str>) -> bool {
    self.reference.is_some_and(|r| r.lock().ok().and_then(|g| g.clone()).as_deref() != open_ref)
  }
}

impl StreamResources {
  pub(crate) fn new() -> Self {
    Self { cap_event: HANDLE::default(), cap_fmt: std::ptr::null_mut(), rnd_fmt: std::ptr::null_mut(), mmcss: None }
  }

  /// Raise this thread to an MMCSS task class; failure only costs glitch resistance.
  pub(crate) fn register_mmcss(&mut self, task: PCWSTR) {
    let mut task_index = 0u32;
    match unsafe { AvSetMmThreadCharacteristicsW(task, &mut task_index) } {
      Ok(h) => self.mmcss = Some(h),
      Err(e) => warn!("[MicStream] MMCSS registration failed (continuing): {}", e),
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
      if !self.rnd_fmt.is_null() {
        CoTaskMemFree(Some(self.rnd_fmt as *const _));
      }
    }
  }
}

/// Sleep in small steps so stop() doesn't wait for a whole backoff period.
fn sleep_while_running(running: &AtomicBool, total: Duration) {
  let step = Duration::from_millis(100);
  let mut slept = Duration::ZERO;
  while slept < total && running.load(Ordering::SeqCst) {
    std::thread::sleep(step);
    slept += step;
  }
}

/// Discovers the virtual microphone render endpoint (eRender)
fn find_virtual_mic_render_endpoint(enumerator: &IMMDeviceEnumerator, explicit_id: Option<&str>) -> Option<IMMDevice> {
  unsafe {
    if let Some(id) = explicit_id {
      let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
      if let Ok(dev) = enumerator.GetDevice(PCWSTR(wide.as_ptr())) {
        return Some(dev);
      }
    }

    // Auto-detect virtual mic render endpoints
    if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
      if let Ok(count) = collection.GetCount() {
        for i in 0..count {
          if let Ok(dev) = collection.Item(i) {
            if let Ok(store) = dev.OpenPropertyStore(windows::Win32::System::Com::STGM_READ) {
              if let Ok(val) = store.GetValue(&crate::win_audio::endpoint_lock::PKEY_DEVICE_FRIENDLY_NAME) {
                if let Ok(pwstr_name) = windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc(&val) {
                  let name_str = pwstr_name.to_string().unwrap_or_default().to_lowercase();
                  CoTaskMemFree(Some(pwstr_name.as_ptr() as *const _));
                  if name_str.contains("sonar - microphone")
                    || name_str.contains("cable input")
                    || name_str.contains("vb-audio")
                    || name_str.contains("virtual audio cable")
                  {
                    info!("[MicStream] Found virtual microphone render sink: {}", name_str);
                    return Some(dev);
                  }
                }
              }
            }
          }
        }
      }
    }

    None
  }
}

/// Runs one continuous WASAPI stream session. Returns Ok(()) when stopped or when the
/// selected devices changed (the caller loop then reopens with the new ids).
fn run_bridge_stream(
  running: &Arc<AtomicBool>,
  wanted: &DeviceSelection<'_>,
  input_id: Option<&str>,
  output_id: Option<&str>,
  reference_id: Option<&str>,
) -> Result<(), String> {
  unsafe {
    let mut res = StreamResources::new();
    // Pro-audio scheduling priority; without it the bridge glitches under CPU load.
    res.register_mmcss(w!("Pro Audio"));

    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
      .map_err(|e| format!("CoCreateInstance MMDeviceEnumerator failed: {}", e))?;

    // 1. Resolve Capture Device (Hardware Microphone)
    let capture_dev: IMMDevice = if let Some(id) = input_id {
      let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
      enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| format!("Failed to get capture device {}: {}", id, e))?
    } else {
      enumerator
        .GetDefaultAudioEndpoint(eCapture, windows::Win32::Media::Audio::eConsole)
        .map_err(|e| format!("Failed to get default capture endpoint: {}", e))?
    };

    // 2. Resolve Render Device (Virtual Microphone Sink)
    let render_dev: IMMDevice =
      find_virtual_mic_render_endpoint(&enumerator, output_id).ok_or_else(|| NO_VIRTUAL_MIC.to_string())?;

    // 3. Initialize Capture Client
    let capture_audio_client: IAudioClient = capture_dev
      .Activate(CLSCTX_ALL, None)
      .map_err(|e| format!("Activate IAudioClient on capture dev failed: {}", e))?;

    res.cap_fmt =
      capture_audio_client.GetMixFormat().map_err(|e| format!("GetMixFormat on capture dev failed: {}", e))?;
    res.cap_event = CreateEventW(None, false, false, None).map_err(|e| format!("CreateEventW failed: {}", e))?;

    let cap = init_client_48k_float(
      &capture_audio_client,
      res.cap_fmt,
      AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
      CAPTURE_BUFFER_MS * HNS_PER_MS,
    )?;

    capture_audio_client.SetEventHandle(res.cap_event).map_err(|e| format!("SetEventHandle failed: {}", e))?;

    let capture_client: IAudioCaptureClient =
      capture_audio_client.GetService().map_err(|e| format!("GetService IAudioCaptureClient failed: {}", e))?;

    // 4. Initialize Render Client (Virtual Mic Sink)
    let render_audio_client: IAudioClient = render_dev
      .Activate(CLSCTX_ALL, None)
      .map_err(|e| format!("Activate IAudioClient on render dev failed: {}", e))?;

    res.rnd_fmt =
      render_audio_client.GetMixFormat().map_err(|e| format!("GetMixFormat on render dev failed: {}", e))?;

    let rnd = init_client_48k_float(
      &render_audio_client,
      res.rnd_fmt,
      0, // push model: written right after each capture packet
      RENDER_BUFFER_MS * HNS_PER_MS,
    )?;

    if rnd.sample_rate != cap.sample_rate {
      // Only when WASAPI refused the 48 kHz request on one side: no resampler here,
      // so pitch/speed will be off. Upgrade: add a resampler if this shows up in logs.
      warn!(
        "[MicStream] Sample rate mismatch: capture {} Hz vs render {} Hz (no resampling)",
        cap.sample_rate, rnd.sample_rate
      );
    }

    let render_client: IAudioRenderClient =
      render_audio_client.GetService().map_err(|e| format!("GetService IAudioRenderClient failed: {}", e))?;

    let render_buffer_size =
      render_audio_client.GetBufferSize().map_err(|e| format!("render_audio_client.GetBufferSize failed: {}", e))?;
    let target_padding = (rnd.sample_rate / 1000 * RENDER_TARGET_MS).min(render_buffer_size / 2);
    let max_padding = (target_padding * 2).min(render_buffer_size);

    // 5. Initialize Pure Rust DSP Plugin
    let mut plugin = GamersEqualizerPlugin::new(cap.sample_rate as f32);
    plugin.mode = PluginMode::MicProcessor;
    // Load the selected noise model (DeepFilterNet ~0.2 s) before audio runs: no gap.
    plugin.preload_selected_model();

    // Warm-start the echo canceller with the delay measured last time for this device pair.
    let cache_path = super::aec_delay_cache::default_path();
    let capture_key_id = input_id.map(str::to_string).or_else(|| device_id_of(&capture_dev));
    let cache_key = match (capture_key_id, super::loopback::resolve_render_id(reference_id)) {
      (Some(c), Some(r)) => Some(super::aec_delay_cache::key(&c, &r)),
      _ => None,
    };
    let mut saved_delay = cache_key.as_deref().and_then(|k| super::aec_delay_cache::load(&cache_path, k));
    if let Some(d) = saved_delay {
      info!("[MicStream] AEC warm start with cached echo delay {:.1} ms", d as f32 / 48.0);
      plugin.set_cached_aec_delay(d);
    }
    let mut blocks_since_save_check: u32 = 0;

    // Pre-fill the cushion so the first packets don't underrun the sink.
    write_silence(&render_client, target_padding).map_err(|e| format!("render prefill: {e}"))?;

    capture_audio_client.Start().map_err(|e| format!("capture_audio_client.Start failed: {}", e))?;
    render_audio_client.Start().map_err(|e| format!("render_audio_client.Start failed: {}", e))?;

    info!(
      "[MicStream] Audio bridge active: Capture [{}ch, {}Hz, {}bit, float={}] -> DSP -> Render [{}ch, {}Hz, float={}], cushion {} frames",
      cap.channels, cap.sample_rate, cap.bits, cap.is_float, rnd.channels, rnd.sample_rate, rnd.is_float, target_padding
    );

    let mut in_mono_buf: Vec<f32> = Vec::with_capacity(1024);
    let mut out_mono_buf: Vec<f32> = Vec::with_capacity(1024);
    let mut trimmed_frames: u64 = 0;
    let mut underruns: u64 = 0;

    while running.load(Ordering::SeqCst) {
      blocks_since_save_check += 1;
      if blocks_since_save_check >= DELAY_SAVE_CHECK_BLOCKS {
        blocks_since_save_check = 0;
        if let (Some(key), Some(d)) = (cache_key.as_deref(), plugin.confirmed_aec_delay()) {
          if saved_delay.is_none_or(|s| s.abs_diff(d) > DELAY_SAVE_MIN_CHANGE) {
            super::aec_delay_cache::save(&cache_path, key, d);
            saved_delay = Some(d);
          }
        }
      }

      if wanted.changed(input_id, output_id) || wanted.reference_changed(reference_id) {
        info!("[MicStream] Device selection changed, reopening stream.");
        break;
      }

      let wait_res = WaitForSingleObject(res.cap_event, 20);
      if wait_res.0 != 0 {
        // Timeout: an invalidated stream (sleep, unplug, audio service restart) often stops
        // signaling, so GetBuffer would never report it. Probe both clients; an error reopens.
        capture_audio_client.GetCurrentPadding().map_err(|e| format!("capture GetCurrentPadding: {e}"))?;
        render_audio_client.GetCurrentPadding().map_err(|e| format!("render GetCurrentPadding: {e}"))?;
        continue;
      }

      loop {
        let mut p_data: *mut u8 = std::ptr::null_mut();
        let mut num_frames_read: u32 = 0;
        let mut flags: u32 = 0;
        let mut qpc: u64 = 0;

        let get_buf_res = capture_client.GetBuffer(&mut p_data, &mut num_frames_read, &mut flags, None, Some(&mut qpc));

        // Error = device invalidated / audio service restarted: reopen instead of idling forever.
        get_buf_res.map_err(|e| format!("capture GetBuffer: {}", e))?;
        if num_frames_read == 0 || p_data.is_null() {
          break;
        }

        let frames = num_frames_read as usize;
        if in_mono_buf.len() < frames {
          in_mono_buf.resize(frames, 0.0);
          out_mono_buf.resize(frames, 0.0);
        }

        // Convert raw capture bytes into normalized [-1.0, 1.0] float mono (first channel)
        #[expect(clippy::indexing_slicing, reason = "both buffers were grown to `frames` above")]
        let mono = &mut in_mono_buf[..frames];
        let ch = cap.channels;
        if (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
          mono.fill(0.0);
        } else if cap.is_float {
          let slice = std::slice::from_raw_parts(p_data as *const f32, frames * ch);
          for (dst, frame) in mono.iter_mut().zip(slice.chunks_exact(ch)) {
            *dst = frame.first().copied().unwrap_or(0.0);
          }
        } else if cap.bits == 16 {
          let slice = std::slice::from_raw_parts(p_data as *const i16, frames * ch);
          for (dst, frame) in mono.iter_mut().zip(slice.chunks_exact(ch)) {
            *dst = frame.first().map_or(0.0, |&v| v as f32 / 32768.0);
          }
        } else if cap.bits == 32 {
          let slice = std::slice::from_raw_parts(p_data as *const i32, frames * ch);
          for (dst, frame) in mono.iter_mut().zip(slice.chunks_exact(ch)) {
            *dst = frame.first().map_or(0.0, |&v| v as f32 / 2_147_483_648.0);
          }
        } else {
          // 24-bit packed etc.: only reachable if the float request was refused.
          mono.fill(0.0);
        }

        // Process through Pure Rust DSP (AEC, nnnoiseless AI, Linear Boost 100%..300%, Limiter)
        #[expect(clippy::indexing_slicing, reason = "both buffers were grown to `frames` above")]
        let (in_slice, mut out_slice): (&[f32], &mut [f32]) = (&in_mono_buf[..frames], &mut out_mono_buf[..frames]);
        let mic_time = (qpc != 0).then_some(qpc);
        plugin.process_timed(&[in_slice], &mut [&mut out_slice], mic_time);

        // Release capture buffer immediately
        let _ = capture_client.ReleaseBuffer(num_frames_read);

        // Render to virtual mic sink, keeping the queued cushion between target and max.
        // Errors (sink reset: Sonar / VB-Cable restart) reopen, else the mic stays silent.
        let render_err = |e: windows::core::Error| format!("render: {e}");
        if render_audio_client.GetCurrentPadding().map_err(render_err)? == 0 {
          // Sink ran dry (mic clock slower than sink): restore the cushion.
          underruns += 1;
          write_silence(&render_client, target_padding).map_err(render_err)?;
        }
        let padding = render_audio_client.GetCurrentPadding().map_err(render_err)?;
        // Mic clock faster than sink: trim instead of letting latency grow without bound.
        // Shortcut: drops samples (rare tick on drift); upgrade to adaptive resampling.
        let frames_to_write = (frames as u32).min(max_padding.saturating_sub(padding));
        trimmed_frames += (frames as u64).saturating_sub(frames_to_write as u64);

        if frames_to_write > 0 {
          {
            let p_rnd_dst = render_client.GetBuffer(frames_to_write).map_err(render_err)?;
            let write_frames = frames_to_write as usize;
            #[expect(clippy::indexing_slicing, reason = "write_frames <= frames <= out_mono_buf.len()")]
            let src = &out_mono_buf[..write_frames];
            let rch = rnd.channels;
            if rnd.is_float {
              let dst_slice = std::slice::from_raw_parts_mut(p_rnd_dst as *mut f32, write_frames * rch);
              for (frame, &sample) in dst_slice.chunks_exact_mut(rch).zip(src) {
                frame.fill(sample);
              }
            } else {
              let dst_slice = std::slice::from_raw_parts_mut(p_rnd_dst as *mut i16, write_frames * rch);
              for (frame, &sample) in dst_slice.chunks_exact_mut(rch).zip(src) {
                frame.fill((sample.clamp(-1.0, 1.0) * 32767.0) as i16);
              }
            }
            render_client.ReleaseBuffer(frames_to_write, 0).map_err(render_err)?;
          }
        }
      }
    }

    if trimmed_frames > 0 || underruns > 0 {
      info!("[MicStream] Session stats: {} frames trimmed (clock drift), {} sink underruns", trimmed_frames, underruns);
    }

    let _ = capture_audio_client.Stop();
    let _ = render_audio_client.Stop();

    Ok(())
  }
}

unsafe fn device_id_of(dev: &IMMDevice) -> Option<String> {
  let pw = dev.GetId().ok()?;
  let id = pw.to_string().ok();
  CoTaskMemFree(Some(pw.as_ptr() as *const _));
  id
}

/// Initialize a shared-mode client at 48 kHz float32 (WASAPI converts and resamples);
/// falls back to the device mix format if the driver refuses.
pub(crate) unsafe fn init_client_48k_float(
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
    Ok(()) => Ok(StreamFormat { channels: channels as usize, sample_rate: DSP_SAMPLE_RATE, bits: 32, is_float: true }),
    Err(e) => {
      warn!("[MicStream] 48 kHz float stream refused ({}), using device mix format", e);
      client
        .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_hns, 0, mix_fmt, None)
        .map_err(|e| format!("IAudioClient.Initialize failed: {}", e))?;
      Ok(StreamFormat {
        channels: (mix.nChannels as usize).max(1),
        sample_rate: mix.nSamplesPerSec,
        bits: mix.wBitsPerSample,
        is_float: is_float_format(mix_fmt),
      })
    }
  }
}

/// Queue `frames` of silence into the render sink.
unsafe fn write_silence(render_client: &IAudioRenderClient, frames: u32) -> windows::core::Result<()> {
  if frames == 0 {
    return Ok(());
  }
  render_client.GetBuffer(frames)?;
  // AUDCLNT_BUFFERFLAGS_SILENT: WASAPI treats the buffer as zeros.
  render_client.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_mic_stream_worker_creation() {
    let worker = MicStreamWorker::new();
    worker.set_input_device(Some("test_device_id".to_string()));
    assert!(!worker.running.load(Ordering::SeqCst));
  }

  #[test]
  fn test_device_selection_detects_change() {
    let input = Mutex::new(Some("mic-a".to_string()));
    let output = Mutex::new(None);
    let sel = DeviceSelection { input: &input, output: &output, reference: None };
    assert!(!sel.changed(Some("mic-a"), None));

    if let Ok(mut g) = input.lock() {
      *g = Some("mic-b".to_string());
    }
    assert!(sel.changed(Some("mic-a"), None), "new mic id must reopen the stream");
  }
}
