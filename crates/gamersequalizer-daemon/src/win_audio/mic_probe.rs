//! `gamersequalizer.exe --probe-mic {guid}` and `--probe-render {guid}`: open an endpoint in
//! shared mode for a couple seconds and exit. The installer runs them right after installing APOs:
//! opening a stream makes Windows start the effect (or not) and logs the outcome to Application
//! event log, while catching any open/init failure or `audiodg.exe` crash immediately.
//! With `--open-only` after the guid (the installer's check before anything changes) a probe only
//! opens the endpoint (Activate, GetMixFormat, Initialize) and exits, no stream: same exit codes.

use log::{info, warn};
use std::ptr::{null_mut, write_bytes};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
  AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_DISABLED, IAudioCaptureClient, IAudioClient,
  IAudioRenderClient,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{Error as WinError, HRESULT};

/// Process exit code: Windows reports the endpoint invalidated (`AUDCLNT_E_DEVICE_INVALIDATED`),
/// e.g. an output switched off in its driver. The installer then does not blame our effects and
/// skips the retry without them (`$ProbeExitDeviceInvalidated` in apo-install.ps1).
pub const EXIT_DEVICE_INVALIDATED: i32 = 2;
/// Trailing argument: open the endpoint only, no stream (the installer's check before installing).
pub const OPEN_ONLY_ARG: &str = "--open-only";

/// Long enough for the audio engine to build the graph and start the capture effect.
const CAPTURE_PROBE_TIME: Duration = Duration::from_secs(3);
/// Output APOs start on the first rendered buffer (~50-150 ms); 1.6 s catches startup reliably.
const RENDER_PROBE_TIME: Duration = Duration::from_millis(1600);
const POLL: Duration = Duration::from_millis(20);
/// Covers the short settling right after an audio restart, when an endpoint can report invalidated
/// for a moment. A device switched off in its driver stays invalidated: it costs this once, then exit 2.
const INVALIDATED_WAIT: Duration = Duration::from_secs(3);
const INVALIDATED_RETRY: Duration = Duration::from_millis(500);
/// 100 ms shared-mode buffer, in 100 ns units.
const BUFFER_HNS: i64 = 1_000_000;
const CAPTURE_ID_PREFIX: &str = "{0.0.1.00000000}.";
const RENDER_ID_PREFIX: &str = "{0.0.0.00000000}.";

/// A failed probe step; `code` is the HRESULT when Windows gave one.
struct ProbeError {
  text: String,
  code: Option<HRESULT>,
}

/// Process exit code: 0 = capture stream ran (opened, with `open_only`), 1 = the microphone could
/// not be opened, `EXIT_DEVICE_INVALIDATED` = Windows reports it invalidated.
pub fn run(guid: &str, open_only: bool) -> i32 {
  let id = endpoint_id(CAPTURE_ID_PREFIX, guid);
  let result = if open_only {
    open_shared_client(&id).map(|_| String::new())
  } else {
    probe_capture(&id).map(|frames| format!(", {frames} frames read"))
  };
  report("microphone", guid, open_only, result)
}

/// Process exit code: 0 = silent shared render stream ran (opened, with `open_only`), 1 = output
/// could not be opened, `EXIT_DEVICE_INVALIDATED` = Windows reports it invalidated.
pub fn run_render(guid: &str, open_only: bool) -> i32 {
  let id = endpoint_id(RENDER_ID_PREFIX, guid);
  let result = if open_only {
    open_shared_client(&id).map(|_| String::new())
  } else {
    probe_render(&id).map(|frames| format!(", {frames} frames written"))
  };
  report("output", guid, open_only, result)
}

/// Logs the outcome (`detail`: what the stream did) and returns the exit code.
fn report(what: &str, guid: &str, open_only: bool, result: Result<String, ProbeError>) -> i32 {
  let mode = if open_only { " (open only)" } else { "" };
  match result {
    Ok(detail) => {
      info!("[Probe] {what} {guid} opened for the install check{mode}{detail}");
      0
    }
    Err(e) => {
      warn!("[Probe] {what} {guid} could not be opened for the install check{mode}: {}", e.text);
      exit_code(&e)
    }
  }
}

fn exit_code(e: &ProbeError) -> i32 {
  if e.code == Some(AUDCLNT_E_DEVICE_INVALIDATED) { EXIT_DEVICE_INVALIDATED } else { 1 }
}

fn endpoint_id(prefix: &str, guid: &str) -> String {
  format!("{prefix}{guid}")
}

/// `about` (device state, then the format tried) goes into the error text, so the log alone says
/// what was opened how.
fn win_err(step: &'static str, about: &str) -> impl FnOnce(WinError) -> ProbeError {
  let about = about.to_string();
  move |e| ProbeError { text: format!("{step}: {e} ({about})"), code: Some(e.code()) }
}

/// The client, its frame size and what was opened (for later errors).
fn open_shared_client(id: &str) -> Result<(IAudioClient, usize, String), ProbeError> {
  // SAFETY: plain WASAPI setup; `mix` is inspected before `CoTaskMemFree` frees it once.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    // Right after Windows audio restarted an endpoint can report "invalidated" for a moment: ask again
    // (with a fresh device object) before calling it failed.
    let give_up = Instant::now() + INVALIDATED_WAIT;
    let (client, about): (IAudioClient, String) = loop {
      let device = super::mmdevice::device(id).map_err(win_err("GetDevice", id))?;
      let state = device.GetState();
      let about = match &state {
        Ok(state) => format!("DeviceState {:#x}", state.0),
        Err(e) => format!("DeviceState unreadable: {e}"),
      };
      // Switched off in Windows: it does not come back on its own, so no waiting. The registry's
      // `DeviceState` (what the installer reads) can still say "active" for it.
      if state.is_ok_and(|s| s == DEVICE_STATE_DISABLED) {
        return Err(ProbeError {
          text: format!("the device is disabled ({about})"),
          code: Some(AUDCLNT_E_DEVICE_INVALIDATED),
        });
      }
      match device.Activate::<IAudioClient>(CLSCTX_ALL, None) {
        Ok(client) => break (client, about),
        Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED && Instant::now() < give_up => {
          std::thread::sleep(INVALIDATED_RETRY);
        }
        Err(e) => return Err(win_err("Activate", &about)(e)),
      }
    };
    let mix = client.GetMixFormat().map_err(win_err("GetMixFormat", &about))?;
    // Copied out: WAVEFORMATEX is packed, its fields cannot be borrowed for `format!`.
    let (tag, channels, rate, bits, block) =
      ((*mix).wFormatTag, (*mix).nChannels, (*mix).nSamplesPerSec, (*mix).wBitsPerSample, (*mix).nBlockAlign);
    let about = format!("{about}, shared mode, mix format tag {tag:#x} {channels} ch {rate} Hz {bits} bit");
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, mix, None);
    CoTaskMemFree(Some(mix as *const _));
    init.map_err(win_err("Initialize", &about))?;
    if block == 0 {
      return Err(ProbeError { text: format!("GetMixFormat returned nBlockAlign == 0 ({about})"), code: None });
    }
    Ok((client, usize::from(block), about))
  }
}

fn probe_capture(id: &str) -> Result<u64, ProbeError> {
  let (client, _, about) = open_shared_client(id)?;
  // SAFETY: `client` is an initialized shared capture client.
  unsafe {
    let capture: IAudioCaptureClient = client.GetService().map_err(win_err("GetService", &about))?;
    client.Start().map_err(win_err("Start", &about))?;
    let end = Instant::now() + CAPTURE_PROBE_TIME;
    let mut frames_read = 0u64;
    let result = loop {
      if Instant::now() >= end {
        break Ok(frames_read);
      }
      std::thread::sleep(POLL);
      match drain(&capture) {
        Ok(frames) => frames_read += frames,
        Err(e) => break Err(streaming_error(e, &about)),
      }
    };
    let _ = client.Stop();
    result
  }
}

fn probe_render(id: &str) -> Result<u64, ProbeError> {
  let (client, block_align, about) = open_shared_client(id)?;
  // SAFETY: `client` is an initialized shared render client with `block_align > 0`.
  unsafe {
    let buf_size = client.GetBufferSize().map_err(win_err("GetBufferSize", &about))?;
    let render: IAudioRenderClient = client.GetService().map_err(win_err("GetService", &about))?;
    client.Start().map_err(win_err("Start", &about))?;
    let end = Instant::now() + RENDER_PROBE_TIME;
    let mut frames_written = 0u64;
    let result = loop {
      if Instant::now() >= end {
        break Ok(frames_written);
      }
      match write_zero_pcm(&client, &render, buf_size, block_align) {
        Ok(frames) => frames_written += frames,
        Err(e) => break Err(streaming_error(e, &about)),
      }
      std::thread::sleep(POLL);
    };
    let _ = client.Stop();
    result
  }
}

/// A failure after the stream ran: the device opened, so never `EXIT_DEVICE_INVALIDATED`.
fn streaming_error(text: String, about: &str) -> ProbeError {
  ProbeError { text: format!("{text} ({about})"), code: None }
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

  /// The installer reads exit 2 as "Windows does not open this device at all".
  #[test]
  fn only_an_invalidated_device_gets_its_own_exit_code() {
    let err = |code| ProbeError { text: String::new(), code };
    assert_eq!(exit_code(&err(Some(AUDCLNT_E_DEVICE_INVALIDATED))), EXIT_DEVICE_INVALIDATED);
    assert_eq!(exit_code(&err(Some(HRESULT(0x8007_0005_u32 as i32)))), 1);
    assert_eq!(exit_code(&err(None)), 1);
  }

  #[test]
  fn silence_byte_len_handles_bounds_without_overflow() {
    assert_eq!(silence_byte_len(480, 8), Some(3840));
    assert_eq!(silence_byte_len(u32::MAX, usize::MAX), None);
  }
}
