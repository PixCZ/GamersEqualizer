//! Sidetone without the UI: keeps the default microphone + default output running and prints
//! what the two APOs report, once per second. Switch sidetone on in the app first.
//!
//!   cargo run -p gamersequalizer-apo --example sidetone_probe -- 20
//!
//! Needs the app's background service running: it mirrors the settings into the APO mapping.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::SHM_NAME;
use gamersequalizer_common::sidetone::device_hash;
use gamersequalizer_mic_chain::SharedMemoryClient;
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
  MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

fn main() -> windows::core::Result<()> {
  let arg = |n: usize| std::env::args().nth(n);
  let secs: u64 = arg(1).and_then(|s| s.parse().ok()).unwrap_or(20);
  let Ok(shm) = SharedMemoryClient::open_existing(SHM_NAME) else {
    println!("the app's background service is not running ({SHM_NAME} missing)");
    return Ok(());
  };
  let h = &shm.layout().header;
  let r = Ordering::Relaxed;
  // SAFETY: straightforward WASAPI calls on this thread; buffers are only handed back.
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let open = |flow: EDataFlow| -> windows::core::Result<(String, IAudioClient)> {
      let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole)?;
      let id = device.GetId()?.to_string().unwrap_or_default();
      let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
      client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, client.GetMixFormat()?, None)?;
      Ok((id, client))
    };
    let (mic_id, mic) = open(eCapture)?;
    let (out_id, out) = open(eRender)?;
    println!("microphone {mic_id}  hash {}", device_hash(&mic_id));
    println!("output     {out_id}  hash {}", device_hash(&out_id));
    let capture: IAudioCaptureClient = mic.GetService()?;
    let render: IAudioRenderClient = out.GetService()?;
    let out_frames = out.GetBufferSize()?;
    mic.Start()?;
    out.Start()?;

    let end = Instant::now() + Duration::from_secs(secs);
    let mut next_print = Instant::now() + Duration::from_secs(1);
    let (mut writer, mut reader) = (h.sidetone_writer_beat.load(r), h.sidetone_reader_beat.load(r));
    // In a closure, so an error in the loop still stops both streams below.
    let outcome = (|| -> windows::core::Result<()> {
      while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
        // Keep the microphone stream drained and the output stream fed with silence.
        while capture.GetNextPacketSize()? > 0 {
          let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
          capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
          capture.ReleaseBuffer(frames)?;
        }
        let free = out_frames.saturating_sub(out.GetCurrentPadding()?);
        if free > 0 {
          render.GetBuffer(free)?;
          render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
        if Instant::now() < next_print {
          continue;
        }
        next_print += Duration::from_secs(1);
        let (w, rd) = (h.sidetone_writer_beat.load(r), h.sidetone_reader_beat.load(r));
        println!(
          "mic: {} blocks/s x {} frames @ {} Hz, hash seen {}, peak {:.3}, gate {:.1} dB, mode {} (+{} us), chain load peak {} permille",
          w.wrapping_sub(writer),
          h.sidetone_mic_block_frames.load(r),
          h.sidetone_mic_rate.load(r),
          h.sidetone_mic_hash_seen.load(r),
          h.sidetone_peak_permille.load(r) as f32 / 1000.0,
          h.sidetone_gate_gain_db_x10.load(r) as f32 / 10.0,
          h.sidetone_noise_mode_active.load(r),
          h.sidetone_noise_delay_us.load(r),
          h.mic_dsp_load_peak_permille.load(r),
        );
        println!(
          "out: {} blocks/s x {} frames @ {} Hz, hash seen {}, queued {} us, age {} us, underruns {}, resyncs {}",
          rd.wrapping_sub(reader),
          h.sidetone_out_block_frames.load(r),
          h.sidetone_out_rate.load(r),
          h.sidetone_output_hash_seen.load(r),
          h.sidetone_queued_us.load(r),
          h.sidetone_age_us.load(r),
          h.sidetone_underruns.load(r),
          h.sidetone_resyncs.load(r),
        );
        (writer, reader) = (w, rd);
      }
      Ok(())
    })();
    let _ = mic.Stop();
    let _ = out.Stop();
    outcome
  }
}
