//! `gamersequalizer.exe --probe-mic {endpoint guid}`: opens a microphone like any app for a
//! few seconds and exits. The installer runs it right after installing the mic chain: opening
//! a stream is what makes Windows start the effect (or not), and the effect notes that in the
//! Application log, which the installer reads to keep the slot that works on this microphone.

use log::{info, warn};
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{
  IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

/// Long enough for the audio engine to build the graph and start the effect.
const PROBE_TIME: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(20);
/// 100 ms shared-mode buffer, in 100 ns units.
const BUFFER_HNS: i64 = 1_000_000;
/// Capture endpoint ids are `{0.0.1.00000000}.{endpoint guid}`.
const CAPTURE_ID_PREFIX: &str = "{0.0.1.00000000}.";

/// Process exit code: 0 = the stream ran, 1 = the microphone could not be opened.
pub fn run(guid: &str) -> i32 {
  match probe(&format!("{CAPTURE_ID_PREFIX}{guid}")) {
    Ok(frames) => {
      info!("[Probe] microphone {guid} opened for the install check, {frames} frames read");
      0
    }
    Err(e) => {
      warn!("[Probe] microphone {guid} could not be opened for the install check: {e}");
      1
    }
  }
}

fn probe(id: &str) -> Result<u64, String> {
  // SAFETY: plain WASAPI calls; the mix format is freed once, every buffer is released.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator =
      CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| format!("GetDevice {id}: {e}"))?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate: {e}"))?;
    let mix = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, mix, None);
    CoTaskMemFree(Some(mix as *const _));
    init.map_err(|e| format!("Initialize: {e}"))?;
    let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("GetService: {e}"))?;
    client.Start().map_err(|e| format!("Start: {e}"))?;
    let end = Instant::now() + PROBE_TIME;
    let mut frames_read = 0u64;
    let result = loop {
      if Instant::now() >= end {
        break Ok(frames_read);
      }
      std::thread::sleep(POLL);
      match drain(&capture) {
        Ok(frames) => frames_read += frames,
        Err(e) => break Err(e),
      }
    };
    let _ = client.Stop();
    result
  }
}

/// Reads and discards every packet that is ready; returns the frames read.
///
/// # Safety
/// `capture` belongs to a started shared-mode capture client.
unsafe fn drain(capture: &IAudioCaptureClient) -> Result<u64, String> {
  let mut total = 0u64;
  while capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))? > 0 {
    let (mut data, mut frames, mut flags) = (null_mut(), 0u32, 0u32);
    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| format!("GetBuffer: {e}"))?;
    total += u64::from(frames);
    capture.ReleaseBuffer(frames).map_err(|e| format!("ReleaseBuffer: {e}"))?;
  }
  Ok(total)
}
