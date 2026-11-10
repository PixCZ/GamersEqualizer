//! Prints the shared-mode engine periods of the default microphone and the default output:
//! the shortest one decides how low the sidetone delay can get.
//!
//!   cargo run -p gamersequalizer-apo --example period_probe

use windows::Win32::Media::Audio::{eCapture, eConsole, eRender, IAudioClient3, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

fn main() -> windows::core::Result<()> {
  // SAFETY: straightforward WASAPI calls on this thread; the format is only read.
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    for (name, flow) in [("microphone", eCapture), ("output", eRender)] {
      let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
      let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
      let format = client.GetMixFormat()?;
      let rate = (*format).nSamplesPerSec;
      let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
      client.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max)?;
      let ms = |frames: u32| frames as f32 * 1000.0 / rate as f32;
      println!("{name}: {}", device.GetId()?.to_string().unwrap_or_default());
      println!("  {rate} Hz, default {default} frames ({:.2} ms), min {min} ({:.2} ms), max {max}, step {fundamental}", ms(default), ms(min));
    }
  }
  Ok(())
}
