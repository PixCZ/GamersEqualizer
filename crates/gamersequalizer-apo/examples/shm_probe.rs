//! Prints what the APO and the daemon see in the APO's Global mapping, once per second.
//!
//!   cargo run -p gamersequalizer-apo --example shm_probe -- 10

use std::sync::atomic::Ordering;
use std::time::Duration;

use gamersequalizer_common::shm::{AecState, APO_SHM_NAME, SHM_NAME};
use gamersequalizer_mic_vst::SharedMemoryClient;
use windows::Win32::System::Performance::QueryPerformanceCounter;

fn main() {
  let secs: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
  let shm = match SharedMemoryClient::open_existing(APO_SHM_NAME) {
    Ok(s) => s,
    Err(e) => {
      println!("APO mapping not available: {e}");
      return;
    }
  };
  let h = &shm.layout().header;
  let r = Ordering::Relaxed;
  let mut last_write = h.write_frame_index.load(r);
  let local = SharedMemoryClient::open_existing(SHM_NAME).ok();
  let local_h = local.as_ref().map(|l| &l.layout().header);
  let mut last_local = local_h.map_or(0, |l| l.write_frame_index.load(r));
  for _ in 0..secs {
    std::thread::sleep(Duration::from_secs(1));
    let mut now = 0i64;
    // SAFETY: out parameter is a local. QPC runs at 10 MHz here (= 100 ns units).
    unsafe {
      let _ = QueryPerformanceCounter(&mut now);
    }
    let write = h.write_frame_index.load(r);
    let snap = h.ref_snap_qpc_hns.load(r);
    println!(
      "tap={} ref_frames/s={} ref_age_ms={:.1} state={:?} delay_ms={:.1} raw_ms={:.1} conf={}‰ est={} ref_db={:.1} mic_db={:.1} out_db={:.1} erle={:.1} seq={} aec_en={}",
      h.tap_active.load(r),
      write.wrapping_sub(last_write),
      (now - snap as i64) as f64 / 10_000.0,
      AecState::from_u32(h.aec_state.load(r)),
      h.aec_delay_us.load(r) as f64 / 1000.0,
      h.aec_raw_delay_us.load(r) as f64 / 1000.0,
      h.aec_confidence_permille.load(r),
      h.aec_estimate_count.load(r),
      h.aec_ref_level_db_x10.load(r) as f64 / 10.0,
      h.aec_mic_level_db_x10.load(r) as f64 / 10.0,
      h.aec_out_level_db_x10.load(r) as f64 / 10.0,
      h.erle_db_times_10.load(r) as f64 / 10.0,
      h.aec_telemetry_seq.load(r),
      h.aec_enabled.load(r),
    );
    last_write = write;
    // The APO processes only while owner_beat moves (~10/s); out_blocks = output APO calls.
    println!(
      "  owner_beat={} out_blocks={} denoise={}",
      h.owner_beat.load(r),
      h.out_apo_blocks.load(r),
      h.denoise_model.load(r)
    );
    if let Some(l) = local_h {
      let w = l.write_frame_index.load(r);
      println!("  daemon ring: tap={} ref_frames/s={}", l.tap_active.load(r), w.wrapping_sub(last_local));
      last_local = w;
    }
  }
}
