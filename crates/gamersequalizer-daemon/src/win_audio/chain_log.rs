//! Debug log of the whole mic chain (`[Chain]`): a start line with the full state when the DSP
//! goes live, one line per tick listing every switch or device/volume change (old -> new), and a
//! summary of the automatically adjusting values every 10 s. Together with `[Leveler]`,
//! `[MicGain]`, `[DSP]` and `[Mic]` it lets a log reconstruct what the chain did without the audio.
//! The DSP runs in audiodg without a logger: its state comes through shared memory (layout v18).

use super::chatmix::{get_endpoint_volume, ChatMixController};
use super::endpoint_lock::EndpointLockManager;
use gamersequalizer_common::shm::{
  AecState, DenoiseLimit, DenoiseModel, ShmHeader, MIC_CHAIN_ASLEEP, MIC_CHAIN_BYPASSED, MIC_CHAIN_LOOKAHEAD,
  MIC_CHAIN_TAKEOVER, NOISE_GATE_AUTO_LOOKAHEAD, NOISE_GATE_OFF,
};
use log::debug;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// No new telemetry for this long = the DSP stopped (same as `dsp_watch.rs`).
const IDLE_AFTER: Duration = Duration::from_secs(2);
const SUMMARY_EVERY: Duration = Duration::from_secs(10);
const FLAGS: [(u32, &str); 4] = [
  (MIC_CHAIN_ASLEEP, "model asleep"),
  (MIC_CHAIN_BYPASSED, "denoiser bypassed"),
  (MIC_CHAIN_TAKEOVER, "takeover"),
  (MIC_CHAIN_LOOKAHEAD, "lookahead"),
];

/// What only the daemon knows: devices and Windows volumes. Read only with debug logging on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainContext {
  pub mic_id: String,
  pub mic_name: String,
  pub default_id: String,
  pub default_name: String,
  /// Windows volume of the mic in use / of the default capture device, % (None = unreadable).
  pub mic_volume_pct: Option<u32>,
  pub default_volume_pct: Option<u32>,
  pub muted: bool,
  /// Windows mic volume the automatic level set, % (None = not acting).
  pub auto_level_pct: Option<u32>,
  /// Mic volume the user asked for (over 100 % = boost), %.
  pub wish_pct: u32,
}

/// DSP state from shared memory, raw values as published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DspSnapshot {
  pub model: u32,
  pub model_active: u32,
  pub limit: u32,
  pub flags: u32,
  pub aec_enabled: bool,
  pub aec_state: u32,
  pub aec_delay_us: u32,
  pub aec_from_cache: bool,
  pub double_talk: bool,
  /// Echo strength (`aec_echo_db_x10`: negative measured, 0 none, positive a bound), AEC3's
  /// fallback echo estimate and the mic gain into AEC3 (dB x 10).
  pub echo_x10: i32,
  pub fallback_echo_x10: i32,
  pub aec_mic_gain_x10: i32,
  pub gate_db: i32,
  pub strength_permille: u32,
  pub eq_version: u32,
  pub clips: u32,
  pub overruns: u32,
  // Summary only.
  pub mic_level_x10: i32,
  pub out_level_x10: i32,
  pub ref_level_x10: i32,
  pub erle_x10: i32,
  pub voice_blocks: u32,
  pub blocks: u32,
  pub in_prob_permille: u32,
  pub out_prob_permille: u32,
  pub soft_prob_permille: u32,
  pub gate_min_x10: i32,
  pub gate_floor_x10: i32,
  pub df_pre_gain_x10: i32,
  pub rescue_blocks: u32,
  pub quiet_ms: u32,
  pub leveler_gain_x10: i32,
  pub leveler_level_x10: i32,
  pub speech_ms: u32,
  pub input_gain_x10: i32,
  pub limiter_min_x10: i32,
  pub load_avg_permille: u32,
  pub load_peak_permille: u32,
  pub latency_us: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainSnapshot {
  pub dsp: DspSnapshot,
  pub ctx: ChainContext,
}

#[derive(Debug, Default)]
pub struct ChainLog {
  seq: Option<u32>,
  seq_changed: Option<Instant>,
  last: Option<ChainSnapshot>,
  interval: Option<Interval>,
}

/// Summary interval: counters at its start, extremes seen in it.
#[derive(Debug)]
struct Interval {
  since: Instant,
  start: DspSnapshot,
  in_max: u32,
  out_max: u32,
  soft_max: u32,
  gate_min: i32,
  limiter_min: i32,
}

impl DspSnapshot {
  pub fn read(h: &ShmHeader) -> Self {
    let r = Ordering::Relaxed;
    Self {
      model: h.denoise_model.load(r),
      model_active: h.denoise_model_active.load(r),
      limit: h.denoise_limit.load(r),
      flags: h.mic_chain_flags.load(r),
      aec_enabled: h.aec_enabled.load(r),
      aec_state: h.aec_state.load(r),
      aec_delay_us: h.aec_delay_us.load(r),
      aec_from_cache: h.aec_delay_from_cache.load(r),
      double_talk: h.aec_double_talk.load(r),
      echo_x10: h.aec_echo_db_x10.load(r),
      fallback_echo_x10: h.aec_fallback_echo_db_x10.load(r),
      aec_mic_gain_x10: h.aec_mic_gain_db_x10.load(r),
      gate_db: h.noise_gate_threshold_db.load(r),
      strength_permille: h.ai_voice_strength_permille.load(r),
      eq_version: h.mic_eq_version.load(r),
      clips: h.mic_input_clips.load(r),
      overruns: h.mic_dsp_overruns.load(r),
      mic_level_x10: h.aec_mic_level_db_x10.load(r),
      out_level_x10: h.aec_out_level_db_x10.load(r),
      ref_level_x10: h.aec_ref_level_db_x10.load(r),
      erle_x10: h.erle_db_times_10.load(r),
      voice_blocks: h.mic_voice_blocks.load(r),
      blocks: h.mic_blocks.load(r),
      in_prob_permille: h.mic_in_prob_max_permille.load(r),
      out_prob_permille: h.mic_out_prob_max_permille.load(r),
      soft_prob_permille: h.mic_soft_prob_max_permille.load(r),
      gate_min_x10: h.mic_gate_gain_min_db_x10.load(r),
      gate_floor_x10: h.mic_gate_floor_db_x10.load(r),
      df_pre_gain_x10: h.mic_df_pre_gain_db_x10.load(r),
      rescue_blocks: h.mic_rescue_blocks.load(r),
      quiet_ms: h.mic_quiet_ms.load(r),
      leveler_gain_x10: h.mic_leveler_gain_db_x10.load(r),
      leveler_level_x10: h.mic_leveler_level_db_x10.load(r),
      speech_ms: h.mic_speech_ms.load(r),
      input_gain_x10: h.mic_input_gain_db_x10.load(r),
      limiter_min_x10: h.mic_limiter_gain_min_db_x10.load(r),
      load_avg_permille: h.mic_dsp_load_avg_permille.load(r),
      load_peak_permille: h.mic_dsp_load_peak_permille.load(r),
      latency_us: h.mic_latency_measured_us.load(r),
    }
  }
}

/// Devices and Windows volumes for the log (COM reads: call only with debug logging on).
pub fn read_context(mix: &ChatMixController, endpoints: &EndpointLockManager, mic_id: Option<&str>) -> ChainContext {
  let devices = endpoints.enumerate_capture_devices();
  let mic_id = mic_id.unwrap_or_default().to_string();
  let default = devices.iter().find(|d| d.is_default);
  let default_id = default.map(|d| d.id.clone()).unwrap_or_default();
  let endpoint_volume = |id: &str| if id.is_empty() { None } else { get_endpoint_volume(id) };
  let pct = |v: f32| (v * 100.0).round().max(0.0) as u32;
  let mic = endpoint_volume(&mic_id);
  ChainContext {
    mic_name: devices.iter().find(|d| d.id == mic_id).map(|d| d.name.clone()).unwrap_or_default(),
    default_name: default.map(|d| d.name.clone()).unwrap_or_default(),
    mic_volume_pct: mic.map(|(v, _)| pct(v)),
    // The same device as the mic in use is not reported twice.
    default_volume_pct: if default_id == mic_id { None } else { endpoint_volume(&default_id).map(|(v, _)| pct(v)) },
    muted: mic.map_or(mix.mic_muted, |(_, muted)| muted),
    auto_level_pct: mix.mic_hw_auto.map(pct),
    wish_pct: pct(mix.mic_volume),
    mic_id,
    default_id,
  }
}

impl ChainLog {
  /// Once per volume sync (~250 ms). `ctx` is only called while debug logging is on and the DSP is live.
  pub fn tick(&mut self, h: &ShmHeader, ctx: impl FnOnce() -> ChainContext) {
    if !log::log_enabled!(log::Level::Debug) {
      *self = Self::default(); // debug off: start fresh when it is switched on
      return;
    }
    let now = Instant::now();
    let seq = h.aec_telemetry_seq.load(Ordering::Acquire);
    if self.seq.is_some_and(|s| s != seq) {
      self.seq_changed = Some(now);
    }
    self.seq = Some(seq);
    if !self.seq_changed.is_some_and(|t| now.duration_since(t) < IDLE_AFTER) {
      // Not live: the next live tick logs a start line again.
      self.last = None;
      self.interval = None;
      return;
    }
    let snap = ChainSnapshot { dsp: DspSnapshot::read(h), ctx: ctx() };
    let summary = self.summary(&snap, now);
    if let Some(line) = self.observe(snap) {
      debug!("[Chain] {line}");
    }
    if let Some(line) = summary {
      debug!("[Chain] {line}");
    }
  }

  /// The start line on the first live tick, then one line with all changes of this tick.
  pub fn observe(&mut self, now: ChainSnapshot) -> Option<String> {
    let Some(prev) = self.last.replace(now.clone()) else {
      return Some(format!("start: {}", state(&now)));
    };
    let changes = changes(&prev, &now);
    (!changes.is_empty()).then(|| format!("changed: {}", changes.join(", ")))
  }

  /// Every `SUMMARY_EVERY`: levels, voice share and the extremes of the interval.
  pub fn summary(&mut self, now: &ChainSnapshot, at: Instant) -> Option<String> {
    let d = &now.dsp;
    let iv = self.interval.get_or_insert_with(|| Interval::new(at, *d));
    iv.in_max = iv.in_max.max(d.in_prob_permille);
    iv.out_max = iv.out_max.max(d.out_prob_permille);
    iv.soft_max = iv.soft_max.max(d.soft_prob_permille);
    iv.gate_min = iv.gate_min.min(d.gate_min_x10);
    iv.limiter_min = iv.limiter_min.min(d.limiter_min_x10);
    if at.duration_since(iv.since) < SUMMARY_EVERY {
      return None;
    }
    let iv = self.interval.replace(Interval::new(at, *d))?;
    let s = &iv.start;
    let blocks = growth(s.blocks, d.blocks);
    let voice = if blocks == 0 {
      "n/a".to_string()
    } else {
      format!("{} %", growth(s.voice_blocks, d.voice_blocks) as u64 * 100 / blocks as u64)
    };
    let c = &now.ctx;
    Some(format!(
      "{:.0} s: mic {} dBFS, out {} dBFS, ref {} dBFS, ERLE {:.1} dB, echo {} (AEC fallback echo {} dB, \
       AEC mic gain {} dB), voice {voice} of {blocks} blocks, \
       prob max in {:.2} out {:.2} Silero {:.2}, gate min {} dB floor {} dB, DF pre-gain {} dB, rescue {} blocks, quiet {} ms, \
       leveler gain {} dB level {} dBFS speech {} ms, input gain {} dB, limiter min {} dB, \
       Windows mic volume {} (default device {}), automatic level {}, clips {}, load avg {:.1} % peak {:.1} %, \
       latency {}",
      at.duration_since(iv.since).as_secs_f32(),
      level(d.mic_level_x10),
      level(d.out_level_x10),
      level(d.ref_level_x10),
      d.erle_x10 as f32 / 10.0,
      echo(d),
      db(d.fallback_echo_x10),
      db(d.aec_mic_gain_x10),
      iv.in_max as f32 / 1000.0,
      iv.out_max as f32 / 1000.0,
      iv.soft_max as f32 / 1000.0,
      db(iv.gate_min),
      db(d.gate_floor_x10),
      db(d.df_pre_gain_x10),
      growth(s.rescue_blocks, d.rescue_blocks),
      d.quiet_ms,
      db(d.leveler_gain_x10),
      level(d.leveler_level_x10),
      growth(s.speech_ms, d.speech_ms),
      db(d.input_gain_x10),
      db(iv.limiter_min),
      volume(c.mic_volume_pct),
      volume(c.default_volume_pct),
      auto_level(c.auto_level_pct),
      d.clips,
      d.load_avg_permille as f32 / 10.0,
      d.load_peak_permille as f32 / 10.0,
      ms(d.latency_us),
    ))
  }
}

impl Interval {
  fn new(since: Instant, start: DspSnapshot) -> Self {
    Self { since, start, in_max: 0, out_max: 0, soft_max: 0, gate_min: 0, limiter_min: 0 }
  }
}

fn changes(prev: &ChainSnapshot, now: &ChainSnapshot) -> Vec<String> {
  let (p, n) = (&prev.dsp, &now.dsp);
  let (pc, nc) = (&prev.ctx, &now.ctx);
  let mut out = Vec::new();
  let mut diff = |label: &str, old: String, new: String| {
    if old != new {
      out.push(format!("{label} {old} -> {new}"));
    }
  };
  diff("model", model(p.model), model(n.model));
  diff("running", model(p.model_active), model(n.model_active));
  diff("limit", limit(p.limit), limit(n.limit));
  for (bit, name) in FLAGS {
    diff(name, on_off(p.flags & bit != 0), on_off(n.flags & bit != 0));
  }
  diff("AEC", aec(p), aec(n));
  diff("AEC delay", ms(p.aec_delay_us), ms(n.aec_delay_us));
  diff("delay from cache", on_off(p.aec_from_cache), on_off(n.aec_from_cache));
  diff("echo", echo(p), echo(n));
  diff("AEC fallback echo", db(p.fallback_echo_x10), db(n.fallback_echo_x10));
  diff("AEC mic gain", db(p.aec_mic_gain_x10), db(n.aec_mic_gain_x10));
  diff("gate", gate(p.gate_db), gate(n.gate_db));
  diff("strength", pct(p.strength_permille), pct(n.strength_permille));
  diff("voice EQ", format!("v{}", p.eq_version), format!("v{}", n.eq_version));
  diff("mic in use", device(&pc.mic_name, &pc.mic_id), device(&nc.mic_name, &nc.mic_id));
  diff("Windows default mic", device(&pc.default_name, &pc.default_id), device(&nc.default_name, &nc.default_id));
  diff("Windows mic volume", volume(pc.mic_volume_pct), volume(nc.mic_volume_pct));
  diff("default device volume", volume(pc.default_volume_pct), volume(nc.default_volume_pct));
  diff("mute", on_off(pc.muted), on_off(nc.muted));
  diff("automatic level", auto_level(pc.auto_level_pct), auto_level(nc.auto_level_pct));
  diff("mic wish", format!("{} %", pc.wish_pct), format!("{} %", nc.wish_pct));
  // A new stream restarts these counters at 0: only growth counts.
  if n.clips > p.clips {
    out.push(format!("input clipped: {} samples at full scale (total {})", n.clips - p.clips, n.clips));
  }
  if n.overruns > p.overruns {
    out.push(format!("{} late blocks (total {})", n.overruns - p.overruns, n.overruns));
  }
  out
}

fn state(s: &ChainSnapshot) -> String {
  let (d, c) = (&s.dsp, &s.ctx);
  let flags: Vec<String> = FLAGS.iter().map(|(bit, name)| format!("{name} {}", on_off(d.flags & bit != 0))).collect();
  format!(
    "mic in use {}, Windows default mic {}, Windows mic volume {} (default device {}), mute {}, automatic level {}, \
     mic wish {} %, model {} (running {}, limit {}), strength {}, gate {}, voice EQ v{}, AEC {} delay {} \
     (from cache {}, double talk {}), echo {} (AEC fallback echo {} dB, AEC mic gain {} dB), {}, clips {}, \
     late blocks {}",
    device(&c.mic_name, &c.mic_id),
    device(&c.default_name, &c.default_id),
    volume(c.mic_volume_pct),
    volume(c.default_volume_pct),
    on_off(c.muted),
    auto_level(c.auto_level_pct),
    c.wish_pct,
    model(d.model),
    model(d.model_active),
    limit(d.limit),
    pct(d.strength_permille),
    gate(d.gate_db),
    d.eq_version,
    aec(d),
    ms(d.aec_delay_us),
    on_off(d.aec_from_cache),
    on_off(d.double_talk),
    echo(d),
    db(d.fallback_echo_x10),
    db(d.aec_mic_gain_x10),
    flags.join(", "),
    d.clips,
    d.overruns,
  )
}

/// Counter growth; a smaller value means a new stream started counting at 0.
fn growth(start: u32, now: u32) -> u32 {
  if now >= start {
    now - start
  } else {
    now
  }
}

fn model(v: u32) -> String {
  format!("{:?}", DenoiseModel::from_u32(v))
}

fn limit(v: u32) -> String {
  format!("{:?}", DenoiseLimit::from_u32(v))
}

fn aec(d: &DspSnapshot) -> String {
  if d.aec_enabled {
    format!("on {:?}", AecState::from_u32(d.aec_state))
  } else {
    "off".to_string()
  }
}

/// Echo strength: how far under the reference its echo reaches the mic.
fn echo(d: &DspSnapshot) -> String {
  match d.echo_x10 {
    0 => "not measured".to_string(),
    v if v > 0 => format!("within the mic's noise (under {:.1} dB)", -v as f32 / 10.0),
    v => format!("{:.1} dB under the reference", v as f32 / 10.0),
  }
}

fn gate(db: i32) -> String {
  match db {
    v if v <= NOISE_GATE_OFF => "off".to_string(),
    v if v >= NOISE_GATE_AUTO_LOOKAHEAD => "auto + lookahead".to_string(),
    _ => "auto".to_string(),
  }
}

fn on_off(v: bool) -> String {
  let text = if v { "on" } else { "off" };
  text.to_string()
}

fn pct(permille: u32) -> String {
  format!("{:.0} %", permille as f32 / 10.0)
}

fn volume(v: Option<u32>) -> String {
  v.map_or("same / unknown".to_string(), |v| format!("{v} %"))
}

fn auto_level(v: Option<u32>) -> String {
  v.map_or("off".to_string(), |v| format!("on at {v} %"))
}

fn device(name: &str, id: &str) -> String {
  match (name.is_empty(), id.is_empty()) {
    (_, true) => "none".to_string(),
    (true, false) => id.to_string(),
    (false, false) => format!("\"{name}\" [{id}]"),
  }
}

fn ms(us: u32) -> String {
  if us == 0 {
    "none".to_string()
  } else {
    format!("{:.1} ms", us as f32 / 1000.0)
  }
}

fn db(x10: i32) -> String {
  format!("{:+.1}", x10 as f32 / 10.0)
}

fn level(x10: i32) -> String {
  if x10 == 0 {
    "none".to_string()
  } else {
    format!("{:.1}", x10 as f32 / 10.0)
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  fn snap() -> ChainSnapshot {
    ChainSnapshot {
      dsp: DspSnapshot { model: 1, model_active: 1, aec_enabled: true, aec_state: 4, gate_db: 1, ..Default::default() },
      ctx: ChainContext {
        mic_id: "{mic}".into(),
        mic_name: "Headset".into(),
        mic_volume_pct: Some(50),
        ..Default::default()
      },
    }
  }

  fn started() -> ChainLog {
    let mut log = ChainLog::default();
    assert!(log.observe(snap()).unwrap().starts_with("start:"));
    log
  }

  #[test]
  fn nothing_changed_nothing_logged() {
    let mut log = started();
    assert_eq!(log.observe(snap()), None);
  }

  #[test]
  fn changes_of_one_tick_go_on_one_line() {
    let mut log = started();
    let mut now = snap();
    now.dsp.model_active = 0;
    now.ctx.mic_volume_pct = Some(62);
    let line = log.observe(now).unwrap();
    assert!(line.contains("running DeepFilterNet -> RnNoise"), "{line}");
    assert!(line.contains("Windows mic volume 50 % -> 62 %"), "{line}");
    assert_eq!(line.lines().count(), 1);
  }

  #[test]
  fn echo_strength_and_what_follows_it_are_logged_as_they_change() {
    let mut log = started();
    let mut now = snap();
    (now.dsp.echo_x10, now.dsp.fallback_echo_x10, now.dsp.aec_mic_gain_x10) = (-215, -220, 120);
    let line = log.observe(now.clone()).unwrap();
    assert!(line.contains("echo not measured -> -21.5 dB under the reference"), "{line}");
    assert!(line.contains("AEC fallback echo +0.0 -> -22.0"), "{line}");
    assert!(line.contains("AEC mic gain +0.0 -> +12.0"), "{line}");
    now.dsp.echo_x10 = 580;
    let line = log.observe(now).unwrap();
    assert!(line.contains("-> within the mic's noise (under -58.0 dB)"), "{line}");
  }

  #[test]
  fn summary_after_10_s_has_the_voice_share_of_the_interval() {
    let mut log = ChainLog::default();
    let t0 = Instant::now();
    let mut s = snap();
    s.dsp.blocks = 100;
    s.dsp.voice_blocks = 10;
    assert_eq!(log.summary(&s, t0), None);
    s.dsp.blocks = 1100;
    s.dsp.voice_blocks = 260;
    assert_eq!(log.summary(&s, t0 + Duration::from_secs(5)), None);
    let line = log.summary(&s, t0 + SUMMARY_EVERY).unwrap();
    assert!(line.contains("voice 25 % of 1000 blocks"), "{line}");
  }
}
