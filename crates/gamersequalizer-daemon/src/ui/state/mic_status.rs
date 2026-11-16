//! What the window shows about the mic chain: running or not, the denoiser's state, latency, load, AEC.

use super::*;

/// Mic DSP counts as running while its telemetry changed within this window (it publishes ~10x/s).
const MIC_DSP_LIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
/// Time a model switch (or stream start) may take before a mismatch counts as a failed load.
const DENOISE_SWITCH_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One snapshot of the echo canceller's telemetry, read from shared memory.
#[derive(Debug, Clone, Default)]
pub struct AecTelemetry {
  pub state: Option<AecState>,
  pub delay_ms: Option<f32>,
  pub raw_delay_ms: Option<f32>,
  pub from_cache: bool,
  pub confidence: f32,
  pub estimate_count: u32,
  pub erle_db: f32,
  pub ref_db: f32,
  pub mic_db: f32,
  pub out_db: f32,
  pub pre_delay_ms: f32,
  pub corr_max_delay_ms: f32,
  pub corr: Vec<f32>,
  pub seq: u32,
  /// A mic stream published telemetry recently. False = no app uses the mic, so the values
  /// (including `state`) are left over from the last stream or still zero.
  pub live: bool,
}

/// Noise suppression as the UI should present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseStatus {
  /// No app is using the microphone: nothing runs; this model starts with the next stream.
  Idle(DenoiseModel),
  Running(DenoiseModel),
  /// Selection changed or stream started moments ago; this one still runs.
  Switching(DenoiseModel),
  /// The DSP reports why the selected model doesn't run (load failed / CPU too slow).
  Limited {
    limit: DenoiseLimit,
    running: DenoiseModel,
  },
  /// Mismatch without a reason from the DSP (e.g. an older APO build).
  Failed {
    selected: DenoiseModel,
    running: DenoiseModel,
  },
}

/// Delay the mic chain adds, ms (see `mic_latency`).
#[derive(Debug, Clone, Copy, Default)]
pub struct MicLatency {
  /// None until the DSP has heard enough speech to measure.
  pub measured_ms: Option<f32>,
  pub aec_ms: f32,
  pub denoise_ms: f32,
  pub lookahead_ms: f32,
}

/// Mic chain CPU time as a share of real time (1.0 = 100 %), last ~100 ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct DspLoad {
  pub avg: f32,
  pub peak: f32,
  /// Late blocks since the stream started.
  pub late_blocks: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AecHistoryPoint {
  pub t: f64,
  pub erle_db: f32,
  pub delay_ms: Option<f32>,
}

impl AppState {
  /// A mic DSP (APO or bridge) published telemetry within the last 2 s. Call once per frame
  /// or more; repeated calls in one frame see the same seq and change nothing.
  pub fn mic_dsp_live(&mut self) -> bool {
    let seq = self.shm_header().map_or(0, |h| h.aec_telemetry_seq.load(Ordering::Acquire));
    let now = std::time::Instant::now();
    match self.mic_dsp_seen.0 {
      None => self.mic_dsp_seen.0 = Some(seq),
      Some(seen) if seen != seq => {
        // First change after being idle = the stream just (re)started and loads its model.
        if self.mic_dsp_seen.1.is_none_or(|t| now.duration_since(t) >= MIC_DSP_LIVE_WINDOW) {
          self.denoise_switched_at = Some(now);
        }
        self.mic_dsp_seen = (Some(seq), Some(now));
      }
      Some(_) => {}
    }
    self.mic_dsp_seen.1.is_some_and(|t| now.duration_since(t) < MIC_DSP_LIVE_WINDOW)
  }

  /// What the noise suppression is really doing. `active` in shared memory is only updated
  /// while a mic DSP (APO or bridge) processes audio, so without one it is stale.
  pub fn denoise_status(&mut self) -> DenoiseStatus {
    let (selected, active) = self.denoise_models();
    let live = self.mic_dsp_live();
    let now = std::time::Instant::now();
    let settling = self.denoise_switched_at.is_some_and(|t| now.duration_since(t) < DENOISE_SWITCH_GRACE);
    let limit =
      self.shm_header().map_or(DenoiseLimit::None, |h| DenoiseLimit::from_u32(h.denoise_limit.load(Ordering::Relaxed)));
    if live && limit != DenoiseLimit::None {
      return DenoiseStatus::Limited { limit, running: active };
    }
    match (live, selected == active) {
      (false, _) => DenoiseStatus::Idle(selected),
      (true, true) => DenoiseStatus::Running(active),
      (true, false) if settling => DenoiseStatus::Switching(active),
      // The audio thread loads the model synchronously; still the old one after the grace period = load failed.
      (true, false) => DenoiseStatus::Failed { selected, running: active },
    }
  }

  /// Delay the mic chain adds: measured end to end in the DSP, and what each stage holds now.
  pub fn mic_latency(&self) -> MicLatency {
    self.shm_header().map_or(MicLatency::default(), |h| {
      let ms = |us: u32| us as f32 / 1000.0;
      let measured = h.mic_latency_measured_us.load(Ordering::Relaxed);
      MicLatency {
        measured_ms: (measured > 0).then(|| ms(measured)),
        aec_ms: ms(h.mic_latency_aec_us.load(Ordering::Relaxed)),
        denoise_ms: ms(h.mic_latency_denoise_us.load(Ordering::Relaxed)),
        lookahead_ms: ms(h.mic_latency_lookahead_us.load(Ordering::Relaxed)),
      }
    })
  }

  pub fn mic_dsp_load(&self) -> DspLoad {
    self.shm_header().map_or(DspLoad::default(), |h| DspLoad {
      avg: h.mic_dsp_load_avg_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      peak: h.mic_dsp_load_peak_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      late_blocks: h.mic_dsp_overruns.load(Ordering::Relaxed),
    })
  }

  /// Read AEC telemetry and append to the plot history when the DSP published new data.
  pub fn poll_aec_telemetry(&mut self) -> AecTelemetry {
    let Some(h) = self.shm_header() else {
      return AecTelemetry::default();
    };
    let us_to_ms = |us: u32| us as f32 / 1000.0;
    let delay_us = h.aec_delay_us.load(Ordering::Relaxed);
    let raw_us = h.aec_raw_delay_us.load(Ordering::Relaxed);
    let mut t = AecTelemetry {
      state: Some(AecState::from_u32(h.aec_state.load(Ordering::Relaxed))),
      delay_ms: (delay_us > 0).then(|| us_to_ms(delay_us)),
      raw_delay_ms: (raw_us > 0).then(|| us_to_ms(raw_us)),
      from_cache: h.aec_delay_from_cache.load(Ordering::Relaxed),
      confidence: h.aec_confidence_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      estimate_count: h.aec_estimate_count.load(Ordering::Relaxed),
      erle_db: h.erle_db_times_10.load(Ordering::Relaxed) as f32 / 10.0,
      ref_db: h.aec_ref_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      mic_db: h.aec_mic_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      out_db: h.aec_out_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      pre_delay_ms: us_to_ms(h.aec_pre_delay_us.load(Ordering::Relaxed)),
      corr_max_delay_ms: us_to_ms(h.aec_corr_max_delay_us.load(Ordering::Relaxed)),
      corr: h.read_aec_corr(),
      seq: h.aec_telemetry_seq.load(Ordering::Acquire),
      live: false,
    };
    t.live = self.mic_dsp_live();
    if t.seq != self.aec_last_seq {
      self.aec_last_seq = t.seq;
      if self.aec_history.len() >= AEC_HISTORY_LEN {
        self.aec_history.pop_front();
      }
      self.aec_history.push_back(AecHistoryPoint {
        t: self.started_at.elapsed().as_secs_f64(),
        erle_db: t.erle_db,
        delay_ms: t.delay_ms,
      });
    }
    t
  }
}
