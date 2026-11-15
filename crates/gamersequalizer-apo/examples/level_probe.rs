//! Measures the default microphone level as apps see it (shared mode, after system effects).
//! Used to verify the APO: compare runs with different `GainDbX10` values.
//!
//!   cargo run -p gamersequalizer-apo --example level_probe -- 4          (default microphone)
//!   cargo run -p gamersequalizer-apo --example level_probe -- 4 render   (loopback of default output)

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
  AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
  MMDeviceEnumerator, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};

const SILENT_FLAG: u32 = 2;

fn main() -> windows::core::Result<()> {
  let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4);
  // SAFETY: straightforward WASAPI calls; buffers are read only within GetBuffer/ReleaseBuffer.
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let render = std::env::args().nth(2).as_deref() == Some("render");
    let device = enumerator.GetDefaultAudioEndpoint(if render { eRender } else { eCapture }, eConsole)?;
    println!("device: {}", device.GetId()?.to_string().unwrap_or_default());
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let format = client.GetMixFormat()?;
    let channels = (*format).nChannels as usize;
    let (rate, bits) = ((*format).nSamplesPerSec, (*format).wBitsPerSample);
    println!("mix format: {rate} Hz, {channels} ch, {bits} bit");
    if (*format).wBitsPerSample != 32 {
      println!("mix format is not 32-bit float; unsupported");
      return Ok(());
    }
    client.Initialize(
      AUDCLNT_SHAREMODE_SHARED,
      if render { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 },
      1_000_000,
      0,
      format,
      None,
    )?;
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;

    let (mut sum, mut count, mut peak) = (0f64, 0u64, 0f32);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
      sleep(Duration::from_millis(10));
      while capture.GetNextPacketSize()? > 0 {
        let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        let n = frames as usize * channels;
        if flags & SILENT_FLAG == 0 && !data.is_null() {
          for &x in std::slice::from_raw_parts(data as *const f32, n) {
            sum += (x as f64) * (x as f64);
            peak = peak.max(x.abs());
          }
        }
        count += n as u64;
        capture.ReleaseBuffer(frames)?;
      }
    }
    client.Stop()?;

    let rms = (sum / count.max(1) as f64).sqrt();
    println!(
      "rms_dbfs={:.1} peak_dbfs={:.1} samples={}",
      20.0 * rms.max(1e-10).log10(),
      20.0 * (peak as f64).max(1e-10).log10(),
      count
    );
  }
  Ok(())
}
