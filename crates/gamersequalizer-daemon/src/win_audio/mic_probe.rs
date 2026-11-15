//! `gamersequalizer.exe --probe-mic {guid}` and `--probe-render {guid}`: open an endpoint in
//! shared mode for a couple seconds and exit. The installer runs them right after installing APOs:
//! opening a stream makes Windows start the effect (or not) and logs the outcome to Application
//! event log, while catching any open/init failure or `audiodg.exe` crash immediately.

use log::{info, warn};
use std::ptr::{null_mut, write_bytes};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
  AUDCLNT_SHAREMODE_SHARED, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
  MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::core::PCWSTR;

/// Long enough for the audio engine to build the graph and start the capture effect.
const CAPTURE_PROBE_TIME: Duration = Duration::from_secs(3);
/// Output APOs start on the first rendered buffer (~50-150 ms); 1.6 s catches startup reliably.
const RENDER_PROBE_TIME: Duration = Duration::from_millis(1600);
const POLL: Duration = Duration::from_millis(20);
/// 100 ms shared-mode buffer, in 100 ns units.
const BUFFER_HNS: i64 = 1_000_000;
const CAPTURE_ID_PREFIX: &str = "{0.0.1.00000000}.";
const RENDER_ID_PREFIX: &str = "{0.0.0.00000000}.";

/// Process exit code: 0 = capture stream ran, 1 = the microphone could not be opened.
pub fn run(guid: &str) -> i32 {
  match probe_capture(&endpoint_id(CAPTURE_ID_PREFIX, guid)) {
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

/// Process exit code: 0 = silent shared render stream ran, 1 = output could not be opened.
pub fn run_render(guid: &str) -> i32 {
  match probe_render(&endpoint_id(RENDER_ID_PREFIX, guid)) {
    Ok(frames) => {
      info!("[Probe] output {guid} opened for the install check, {frames} frames written");
      0
    }
    Err(e) => {
      warn!("[Probe] output {guid} could not be opened for the install check: {e}");
      1
    }
  }
}

fn endpoint_id(prefix: &str, guid: &str) -> String {
  format!("{prefix}{guid}")
}

fn open_shared_client(id: &str) -> Result<(IAudioClient, usize), String> {
  // SAFETY: plain WASAPI setup; `mix` is inspected before `CoTaskMemFree` frees it once.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator =
      CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(|e| format!("GetDevice {id}: {e}"))?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("Activate: {e}"))?;
    let mix = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
    let block_align = usize::from((*mix).nBlockAlign);
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, mix, None);
    CoTaskMemFree(Some(mix as *const _));
    init.map_err(|e| format!("Initialize: {e}"))?;
    if block_align == 0 {
      return Err("GetMixFormat returned nBlockAlign == 0".to_string());
    }
    Ok((client, block_align))
  }
}

fn probe_capture(id: &str) -> Result<u64, String> {
  let (client, _) = open_shared_client(id)?;
  // SAFETY: `client` is an initialized shared capture client.
  unsafe {
    let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("GetService: {e}"))?;
    client.Start().map_err(|e| format!("Start: {e}"))?;
    let end = Instant::now() + CAPTURE_PROBE_TIME;
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

fn probe_render(id: &str) -> Result<u64, String> {
  let (client, block_align) = open_shared_client(id)?;
  // SAFETY: `client` is an initialized shared render client with `block_align > 0`.
  unsafe {
    let buf_size = client.GetBufferSize().map_err(|e| format!("GetBufferSize: {e}"))?;
    let render: IAudioRenderClient = client.GetService().map_err(|e| format!("GetService: {e}"))?;
    client.Start().map_err(|e| format!("Start: {e}"))?;
    let end = Instant::now() + RENDER_PROBE_TIME;
    let mut frames_written = 0u64;
    let result = loop {
      if Instant::now() >= end {
        break Ok(frames_written);
      }
      match write_zero_pcm(&client, &render, buf_size, block_align) {
        Ok(frames) => frames_written += frames,
        Err(e) => break Err(e),
      }
      std::thread::sleep(POLL);
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
  // SAFETY: the contract above; each packet is released before the next one is asked for.
  unsafe {
    let mut total = 0u64;
    while capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))? > 0 {
      let (mut data, mut frames, mut flags) = (null_mut(), 0u32, 0u32);
      capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| format!("GetBuffer: {e}"))?;
      total += u64::from(frames);
      capture.ReleaseBuffer(frames).map_err(|e| format!("ReleaseBuffer: {e}"))?;
    }
    Ok(total)
  }
}

/// Fills available render buffer frames with zero PCM samples (flags = 0 so the audio engine
/// exercises the full APO graph rather than skipping silent blocks).
///
/// # Safety
/// `client` and `render` belong to a started shared-mode render client with `block_align > 0`.
unsafe fn write_zero_pcm(
  client: &IAudioClient,
  render: &IAudioRenderClient,
  buf_size: u32,
  block_align: usize,
) -> Result<u64, String> {
  // SAFETY: the contract above; GetBuffer hands out `avail` frames, `byte_len` is `avail * block_align` bytes
  // (overflow checked), written as zeros before the release.
  unsafe {
    let pad = client.GetCurrentPadding().map_err(|e| format!("GetCurrentPadding: {e}"))?;
    let avail = buf_size.saturating_sub(pad);
    if avail == 0 {
      return Ok(0);
    }
    let byte_len =
      silence_byte_len(avail, block_align).ok_or_else(|| "render buffer byte count overflow".to_string())?;
    let data = render.GetBuffer(avail).map_err(|e| format!("GetBuffer: {e}"))?;
    write_bytes(data, 0, byte_len);
    render.ReleaseBuffer(avail, 0).map_err(|e| format!("ReleaseBuffer: {e}"))?;
    Ok(u64::from(avail))
  }
}

fn silence_byte_len(frames: u32, block_align: usize) -> Option<usize> {
  usize::try_from(frames).ok()?.checked_mul(block_align)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn endpoint_ids_use_expected_flow_prefixes() {
    assert_eq!(endpoint_id(CAPTURE_ID_PREFIX, "{1234}"), "{0.0.1.00000000}.{1234}");
    assert_eq!(endpoint_id(RENDER_ID_PREFIX, "{abcd}"), "{0.0.0.00000000}.{abcd}");
  }

  #[test]
  fn silence_byte_len_handles_bounds_without_overflow() {
    assert_eq!(silence_byte_len(480, 8), Some(3840));
    assert_eq!(silence_byte_len(u32::MAX, usize::MAX), None);
  }
}
