//! `ShmHeader`: the settings and telemetry both sides read, and its plain accessors.

use super::*;

/// Lock-free header inside the Windows shared memory mapping
#[repr(C)]
pub struct ShmHeader {
  pub magic: u32,
  pub version: u32,

  // Single-producer single-consumer ring buffer head/tail
  pub write_frame_index: AtomicU32,

  // Runtime control flags updated by GUI / Daemon
  pub aec_enabled: AtomicBool,
  pub ai_voice_strength_permille: AtomicU32, // 0 to 1000 (0.0 to 1.0)
  pub noise_gate_threshold_db: AtomicI32,    // NOISE_GATE_AUTO[_LOOKAHEAD] or NOISE_GATE_OFF

  // Telemetry fed back by VST to GUI
  pub erle_db_times_10: AtomicI32, // e.g. 185 = 18.5 dB echo return loss enhancement
  pub tap_active: AtomicBool,
  pub mic_active: AtomicBool,

  // Multi-process IPC between Background Daemon and Standalone UI Tool
  pub daemon_active: AtomicBool,
  pub shutdown_daemon_signal: AtomicBool,
  pub master_volume_permille: AtomicU32,
  pub game_volume_permille: AtomicU32,
  pub chat_volume_permille: AtomicU32,
  pub media_volume_permille: AtomicU32,
  pub mic_volume_permille: AtomicU32,
  pub master_muted: AtomicBool,
  pub game_muted: AtomicBool,
  pub chat_muted: AtomicBool,
  pub media_muted: AtomicBool,
  pub mic_muted: AtomicBool,
  pub chatmix_balance_permille: AtomicI32,
  pub selected_mic_id: [u16; 64],
  pub mic_id_version: AtomicU32,

  // --- v3: microphone EQ (GUI -> DSP) ---
  pub mic_eq_version: AtomicU32,
  pub mic_eq_band_count: AtomicU32,
  pub mic_eq_bands: [ShmEqBand; MAX_EQ_BANDS],

  // --- v3: noise suppression model (GUI -> DSP, DSP reports what actually runs) ---
  pub denoise_model: AtomicU32,
  pub denoise_model_active: AtomicU32,

  // --- v3: reference timing (loopback writer -> DSP), guarded by a seqlock ---
  /// Odd while the writer updates the snapshot.
  pub ref_seq: AtomicU32,
  /// Ring index whose frame plays at `ref_snap_qpc_hns`.
  pub ref_snap_index: AtomicU32,
  /// QPC time in 100 ns units; 0 = no reference captured yet.
  pub ref_snap_qpc_hns: AtomicU64,

  // --- v3: AEC telemetry (DSP -> GUI) ---
  pub aec_state: AtomicU32,
  pub aec_delay_us: AtomicU32,
  pub aec_raw_delay_us: AtomicU32,
  pub aec_confidence_permille: AtomicU32,
  pub aec_estimate_count: AtomicU32,
  pub aec_delay_from_cache: AtomicBool,
  pub aec_ref_level_db_x10: AtomicI32,
  pub aec_mic_level_db_x10: AtomicI32,
  pub aec_out_level_db_x10: AtomicI32,
  pub aec_pre_delay_us: AtomicU32,
  pub aec_telemetry_seq: AtomicU32,
  /// Cross-correlation magnitude over 0..max delay (f32 bits).
  pub aec_corr: [AtomicU32; AEC_CORR_POINTS],
  pub aec_corr_max_delay_us: AtomicU32,

  // --- v4: echo delay remembered by the daemon (daemon -> DSP), 48 kHz samples, 0 = none.
  // The APO (LOCAL SERVICE) can't read the user's cache file, so it gets the value here.
  pub aec_cached_delay_samples: AtomicU32,
  // --- v12: the reference output changed (daemon -> DSP). Bumped *after* the new output's
  // `aec_cached_delay_samples` is stored: the DSP drops its delay lock and starts from that.
  pub aec_reference_seq: AtomicU32,
  /// DSP -> daemon: the `aec_reference_seq` the reported delay belongs to. The daemon saves a
  /// delay only when this matches, so the old output's delay never lands under the new key.
  pub aec_reference_seq_seen: AtomicU32,

  // --- v5: master switch + output EQ (headphone correction + game profile) ---
  /// Everything off: the output EQ and the mic chain pass audio through unchanged.
  pub master_bypass: AtomicBool,
  /// Combined output EQ (only the enabled layers, headphone first, then game).
  pub out_eq_version: AtomicU32,
  pub out_eq_band_count: AtomicU32,
  pub out_preamp_db_bits: AtomicU32,
  pub out_eq_bands: [ShmEqBand; MAX_OUT_EQ_BANDS],
  /// Bumped after output EQ settings/profiles were saved: other processes reload them.
  pub out_settings_version: AtomicU32,
  /// Game whose profile is active, written by the process that runs game detection.
  /// Seqlock: odd while being written.
  pub active_game_seq: AtomicU32,
  pub active_game_profile: [AtomicU16; GAME_TEXT_LEN],
  pub active_game_label: [AtomicU16; GAME_TEXT_LEN],
  /// Output APO telemetry (APO -> UI): blocks processed (heartbeat), stream format.
  pub out_apo_blocks: AtomicU32,
  pub out_apo_sample_rate: AtomicU32,
  pub out_apo_channels: AtomicU32,

  // --- v6: mic DSP CPU load (DSP -> GUI) ---
  /// Processing time / block duration, last telemetry window: peak and average (1000 = 100 %).
  pub mic_dsp_load_peak_permille: AtomicU32,
  pub mic_dsp_load_avg_permille: AtomicU32,
  /// Blocks (cumulative) whose processing took longer than the block lasts = audible glitch risk.
  pub mic_dsp_overruns: AtomicU32,
  pub denoise_limit: AtomicU32,

  // --- v8: stream EQ per channel (daemon -> stream APO), `StreamBus::WITH_EQ` order ---
  pub stream_eq: [ShmStreamEq; STREAM_EQ_BUSES],

  // --- v9: owner heartbeat (daemon of the active console session -> APO) ---
  /// Bumped by the one daemon whose settings the APO follows. Unchanged for `OWNER_TIMEOUT`
  /// (nobody runs the app, it exited or crashed, another Windows user is at the console) =
  /// the APO passes all audio through untouched: it never applies stale or someone else's settings.
  pub owner_beat: AtomicU32,

  // --- v10: learning voice leveler ---
  /// GUI -> DSP: bring speech to one level (after the voice EQ, before boost/limiter).
  pub mic_leveler_enabled: AtomicBool,
  /// DSP -> GUI: gain the leveler applies now, dB x 10.
  pub mic_leveler_gain_db_x10: AtomicI32,
  /// GUI -> DSP: output loudness the user wants, dBFS x 10.
  pub mic_leveler_target_db_x10: AtomicI32,
  /// GUI -> DSP: bumped by "Learn again" (forget the learned level).
  pub mic_leveler_reset_seq: AtomicU32,
  /// Daemon -> DSP: level learned earlier for this microphone, dBFS x 10 (0 = none).
  pub mic_leveler_saved_level_db_x10: AtomicI32,
  /// DSP -> daemon: level learned so far, dBFS x 10 (0 = none yet); the daemon saves it per mic.
  pub mic_leveler_level_db_x10: AtomicI32,

  // --- v11: mic volume around the leveler, automatic input gain ---
  /// Daemon -> DSP: gain of the Windows mic volume (applied before the APO), dB x 10. The leveler
  /// learns the level without it, so a volume change is compensated at once (0 = 0 dB / unknown).
  pub mic_input_gain_db_x10: AtomicI32,
  /// DSP -> daemon: raw input samples at full scale (cumulative): the converter clipped.
  pub mic_input_clips: AtomicU32,
  /// DSP -> daemon: speech the leveler learned from (cumulative ms).
  pub mic_speech_ms: AtomicU32,
  /// Daemon -> GUI: Windows mic volume the automatic input gain set, permille (0 = none). The
  /// window must not read it as the user's volume.
  pub mic_auto_gain_permille: AtomicU32,

  // --- v13: timing of the audio engine's calls into the mic APO (APO -> daemon debug log) ---
  /// Largest gap between the interval of two calls and the block length, last ~1 s, in us.
  pub mic_call_jitter_peak_us: AtomicU32,
  /// Calls (cumulative) that came over 1.5 blocks after the previous one: the stream stalled
  /// and apps got their data late.
  pub mic_call_gaps: AtomicU32,

  // --- v14: delay the mic chain adds (DSP -> GUI/log), microseconds ---
  /// Measured end to end (raw input vs final output); 0 = not measured yet.
  pub mic_latency_measured_us: AtomicU32,
  /// What each stage holds right now: echo canceller, noise suppression model, gate lookahead.
  pub mic_latency_aec_us: AtomicU32,
  pub mic_latency_denoise_us: AtomicU32,
  pub mic_latency_lookahead_us: AtomicU32,

  // --- v15: try the noise model again (GUI -> DSP) ---
  /// Bumped when the user picks the selected model again: one that failed to load or was too
  /// slow gets another try (`denoise_model` alone can't say "again").
  pub denoise_retry_seq: AtomicU32,

  // --- v16: chain telemetry for the debug log (DSP -> daemon) ---
  /// Blocks (cumulative) in which the voice detector heard speech; with `mic_blocks` the voice share.
  pub mic_voice_blocks: AtomicU32,
  /// Blocks the mic chain processed (cumulative; a new stream starts at 0).
  pub mic_blocks: AtomicU32,
  /// Highest raw-input / output voice probability over the DSP's last ~250 ms, permille (0 = none).
  pub mic_in_prob_max_permille: AtomicU32,
  pub mic_out_prob_max_permille: AtomicU32,
  /// v17: the same for Silero VAD on the raw input, the gate's second key.
  pub mic_soft_prob_max_permille: AtomicU32,
  /// Lowest gate gain over the last ~250 ms, dB x 10 (0 = gate off or open all the time).
  pub mic_gate_gain_min_db_x10: AtomicI32,
  /// Closed-gate floor now, incl. the leveler extra, dB x 10 (0 = gate off).
  pub mic_gate_floor_db_x10: AtomicI32,
  /// DeepFilterNet pre-gain now, dB x 10.
  pub mic_df_pre_gain_db_x10: AtomicI32,
  /// Blocks (cumulative) in which the rescue path was mixed in over half.
  pub mic_rescue_blocks: AtomicU32,
  /// `MIC_CHAIN_*` bits: model asleep, denoiser bypassed, takeover, lookahead on.
  pub mic_chain_flags: AtomicU32,
  /// Time since the denoiser last saw speech or activity (`quiet_samples`), ms.
  pub mic_quiet_ms: AtomicU32,
  /// Lowest peak limiter gain over the last ~250 ms, dB x 10 (0 = no reduction).
  pub mic_limiter_gain_min_db_x10: AtomicI32,

  // --- v18: echo strength, i.e. how many dB under the reference its echo reaches the mic ---
  /// Daemon -> DSP: the strength remembered for this mic and output, dB x 10 (0 = none). Like
  /// `aec_cached_delay_samples`, stored before `aec_reference_seq` is bumped.
  pub aec_cached_echo_db_x10: AtomicI32,
  /// DSP -> daemon: the strength confirmed, or still the remembered one, dB x 10: negative
  /// (-1 at most); 0 = none yet; positive = only a bound: no echo was seen following the
  /// reference, there is at most one minus this value under it (it may be the user's voice).
  /// One field, so the daemon never pairs a value with the wrong kind.
  pub aec_echo_db_x10: AtomicI32,
  /// DSP -> daemon: AEC3's fallback echo estimate now, dB x 10 under the reference (-400 unless
  /// a stronger echo was measured), and the mic gain into AEC3 now, dB x 10.
  pub aec_fallback_echo_db_x10: AtomicI32,
  pub aec_mic_gain_db_x10: AtomicI32,
  /// v19, DSP -> daemon: how the mic's level followed the reference's in the echo meter's last
  /// fit, correlation and slope x 1000 (an echo is confirmed from 900 and a slope of 700-1300).
  pub aec_echo_fit_permille: AtomicI32,
  pub aec_echo_slope_permille: AtomicI32,

  // --- v20: sidetone (own voice in the headphones), see `sidetone/` ---
  /// Daemon -> APOs: sidetone is on for the current microphone + output pair. The values
  /// below are that pair's (`write_sidetone` / `read_sidetone`).
  pub sidetone_enabled: AtomicBool,
  /// `NoiseMode::to_u32`.
  pub sidetone_noise_mode: AtomicU32,
  /// 1000 = 100 % (`SidetoneSettings::volume`).
  pub sidetone_volume_permille: AtomicU32,
  pub sidetone_gate_margin_db_x10: AtomicI32,
  pub sidetone_gate_range_db_x10: AtomicI32,
  /// v21: the pair's parametric EQ, published like the microphone EQ (`write_sidetone_eq`).
  pub sidetone_eq_version: AtomicU32,
  pub sidetone_eq_band_count: AtomicU32,
  pub sidetone_eq_bands: [ShmEqBand; MAX_EQ_BANDS],
  /// Daemon -> APOs: `device_hash` of the pair's microphone and output, 0 = none. An APO
  /// takes part only when the hash of its own endpoint matches.
  pub sidetone_mic_hash: AtomicU32,
  pub sidetone_output_hash: AtomicU32,
  /// GUI -> daemon: counts up while "Try it" is on and the Sidetone page is open; the daemon
  /// keeps the microphone open while it changes.
  pub sidetone_test_beat: AtomicU32,
  /// GUI -> daemon: ask the audio engine for its shortest period on both devices.
  pub sidetone_low_latency: AtomicBool,
  /// Mic APO -> daemon: blocks written to the sidetone ring (changes = sidetone is fed).
  pub sidetone_writer_beat: AtomicU32,
  /// Mic APO -> daemon: its stream's rate and the frames of its last block (= engine period).
  pub sidetone_mic_rate: AtomicU32,
  pub sidetone_mic_block_frames: AtomicU32,
  /// Mic APO -> daemon: peak of the sidetone signal over the last ~100 ms, x 1000 (1000 =
  /// full scale), and the gain of the most closed expander band, dB x 10.
  pub sidetone_peak_permille: AtomicU32,
  pub sidetone_gate_gain_db_x10: AtomicI32,
  /// Mic APO -> daemon: the noise mode that really runs (`NoiseMode::to_u32`; the expander
  /// stands in when a frame-based one can't) and the delay it adds, microseconds.
  pub sidetone_noise_mode_active: AtomicU32,
  pub sidetone_noise_delay_us: AtomicU32,
  /// Mic APO -> daemon: `device_hash` of the endpoint it sits on, 0 = it could not tell.
  pub sidetone_mic_hash_seen: AtomicU32,
  /// Output APO -> daemon: blocks mixed in (changes = sidetone is playing).
  pub sidetone_reader_beat: AtomicU32,
  pub sidetone_out_rate: AtomicU32,
  pub sidetone_out_block_frames: AtomicU32,
  /// Output APO -> daemon: audio queued in the ring after its last read, and the age of the
  /// sample it mixed last (capture to mix), microseconds; age 0 = unknown.
  pub sidetone_queued_us: AtomicU32,
  pub sidetone_age_us: AtomicU32,
  /// Output APO -> daemon: times it ran dry / jumped ahead (cumulative).
  pub sidetone_underruns: AtomicU32,
  pub sidetone_resyncs: AtomicU32,
  /// Output APO -> daemon: `device_hash` of the endpoint it sits on, 0 = it could not tell.
  pub sidetone_output_hash_seen: AtomicU32,

  // --- v23: speaker reference written by the output APO (`reference_ring.rs`) ---
  /// Daemon -> output APOs (mirrored with the controls): `device_hash` of the output whose APO
  /// writes the reference ring; 0 = nobody writes (no app records through the mic chain, the
  /// output has no effect, or the loopback is forced).
  pub aec_ref_output_hash: AtomicU32,
  /// Output APO -> daemon: bumped on every block the one writer puts into the ring.
  pub aec_ref_writer_beat: AtomicU32,
  /// Output APO -> daemon: bumped by an output instance that cannot tell its endpoint while a
  /// reference is wanted; it never writes, and the window says why.
  pub aec_ref_blind_beat: AtomicU32,
  /// Daemon -> window: `AecRefStatus::to_u32`, where the reference comes from or why there is none.
  pub aec_ref_status: AtomicU32,
  /// Output APO instances: which one writes the ring (`process id << 32 | instance id`, 0 =
  /// none), taken by compare-exchange; another takes over once the writer beat stood still.
  pub aec_ref_writer_id: AtomicU64,
}

/// Game detected by the detection owner (daemon, or the GUI without a daemon).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveGame {
  /// Game profile name; empty = detected game without a profile.
  pub profile: String,
  /// What was detected ("Tom Clancy's The Division 2 (division2.exe)").
  pub label: String,
}

impl Default for ShmHeader {
  /// What a fresh mapping holds: zeroed memory, then `init_defaults`.
  fn default() -> Self {
    // SAFETY: every field is an integer, an atomic integer or bool, or an array of them, so all-zero bytes are a
    // valid value.
    let mut h: Self = unsafe { std::mem::zeroed() };
    h.init_defaults();
    h
  }
}

impl ShmHeader {
  pub fn set_selected_mic_id(&mut self, id: &str) {
    let mut arr = [0u16; 64];
    for (slot, c) in arr.iter_mut().zip(id.encode_utf16().take(63)) {
      *slot = c;
    }
    self.selected_mic_id = arr;
    self.mic_id_version.fetch_add(1, Ordering::SeqCst);
  }

  pub fn get_selected_mic_id(&self) -> Option<String> {
    let len = self.selected_mic_id.iter().position(|&c| c == 0).unwrap_or(64);
    if len == 0 { None } else { self.selected_mic_id.get(..len).and_then(|s| String::from_utf16(s).ok()) }
  }

  /// None when this build's layout set the mapping up (checked after `magic`), else why it cannot
  /// be used, with both versions.
  pub fn layout_mismatch(&self) -> Option<String> {
    fence(Ordering::Acquire);
    (self.version != SHM_VERSION).then(|| format!("layout version {} (this build: {SHM_VERSION})", self.version))
  }

  pub fn init_defaults(&mut self) {
    // `version` before `magic`: a process that sees the magic of a mapping another one (several APO
    // instances at once) is still setting up must not read a zero version as another layout.
    self.version = SHM_VERSION;
    fence(Ordering::Release);
    self.magic = SHM_MAGIC;
    self.aec_enabled.store(true, Ordering::Relaxed);
    self.ai_voice_strength_permille.store(750, Ordering::Relaxed);
    self.noise_gate_threshold_db.store(NOISE_GATE_AUTO_LOOKAHEAD, Ordering::Relaxed);
    self.master_volume_permille.store(1000, Ordering::Relaxed);
    self.game_volume_permille.store(1000, Ordering::Relaxed);
    self.chat_volume_permille.store(1000, Ordering::Relaxed);
    self.media_volume_permille.store(1000, Ordering::Relaxed);
    self.mic_volume_permille.store(1000, Ordering::Relaxed);
    self.master_muted.store(false, Ordering::Relaxed);
    self.game_muted.store(false, Ordering::Relaxed);
    self.chat_muted.store(false, Ordering::Relaxed);
    self.media_muted.store(false, Ordering::Relaxed);
    self.mic_muted.store(false, Ordering::Relaxed);
    self.chatmix_balance_permille.store(0, Ordering::Relaxed);
    // Silence until the first telemetry, not 0 dB.
    self.aec_ref_level_db_x10.store(-1200, Ordering::Relaxed);
    self.aec_mic_level_db_x10.store(-1200, Ordering::Relaxed);
    self.aec_out_level_db_x10.store(-1200, Ordering::Relaxed);
    // Best quality at ~1 % CPU; the DSP falls back to RNNoise if it fails to load or overloads.
    self.denoise_model.store(DenoiseModel::DeepFilterNet.to_u32(), Ordering::Relaxed);
    self.mic_leveler_enabled.store(true, Ordering::Relaxed);
    self.mic_leveler_target_db_x10.store(MIC_LEVELER_DEFAULT_TARGET_DB_X10, Ordering::Relaxed);
    // Off, with the default values: a pair's settings replace them when one is active.
    self.write_sidetone(&SidetoneSettings::default());
    // Nobody writes the reference until the daemon names the output.
    self.aec_ref_output_hash.store(0, Ordering::Relaxed);
    self.aec_ref_status.store(AecRefStatus::NotChecked.to_u32(), Ordering::Relaxed);
  }

  pub fn write_active_game(&self, game: Option<&ActiveGame>) {
    let empty = ActiveGame::default();
    let game = game.unwrap_or(&empty);
    seq_write(&self.active_game_seq, || {
      write_text(&self.active_game_profile, &game.profile);
      write_text(&self.active_game_label, &game.label);
    });
  }

  /// None = no game active. Retries while a writer is mid-update.
  pub fn read_active_game(&self) -> Option<ActiveGame> {
    for _ in 0..16 {
      let s1 = self.active_game_seq.load(Ordering::Acquire);
      if s1 & 1 == 1 {
        std::hint::spin_loop();
        continue;
      }
      let game =
        ActiveGame { profile: read_text(&self.active_game_profile), label: read_text(&self.active_game_label) };
      // The text loads are relaxed: keep them before the second seq load (ARM64).
      fence(Ordering::Acquire);
      if self.active_game_seq.load(Ordering::Relaxed) == s1 {
        return (!game.label.is_empty()).then_some(game);
      }
    }
    None
  }

  /// Publish the sidetone settings of the current pair (daemon). `enabled` goes last, so an
  /// APO never runs a fresh "on" with the previous pair's values.
  pub fn write_sidetone(&self, settings: &SidetoneSettings) {
    self.write_sidetone_values(settings);
    self.sidetone_enabled.store(settings.enabled, Ordering::Release);
  }

  /// The values of `write_sidetone` without `enabled`: for live edits by the window, which must
  /// not race the daemon's own off/on around a pair switch.
  pub fn write_sidetone_values(&self, settings: &SidetoneSettings) {
    let r = Ordering::Relaxed;
    let s = settings.clamped();
    let x10 = |db: f32| (db * 10.0).round() as i32;
    self.sidetone_noise_mode.store(s.noise_mode.to_u32(), r);
    self.sidetone_volume_permille.store((s.volume * 1000.0).round() as u32, r);
    self.sidetone_gate_margin_db_x10.store(x10(s.gate_margin_db), r);
    self.sidetone_gate_range_db_x10.store(x10(s.gate_range_db), r);
  }

  /// The sidetone settings as the APOs use them (always inside their ranges).
  pub fn read_sidetone(&self) -> SidetoneSettings {
    let r = Ordering::Relaxed;
    let db = |v: &AtomicI32| v.load(r) as f32 / 10.0;
    SidetoneSettings {
      enabled: self.sidetone_enabled.load(Ordering::Acquire),
      noise_mode: NoiseMode::from_u32(self.sidetone_noise_mode.load(r)),
      volume: self.sidetone_volume_permille.load(r) as f32 / 1000.0,
      gate_margin_db: db(&self.sidetone_gate_margin_db_x10),
      gate_range_db: db(&self.sidetone_gate_range_db_x10),
    }
    .clamped()
  }

  pub fn read_aec_corr(&self) -> Vec<f32> {
    self.aec_corr.iter().map(|a| f32::from_bits(a.load(Ordering::Relaxed))).collect()
  }
}

/// NUL-terminated UTF-16, truncated to the slot count minus one.
/// Seqlock write (one writer): the counter is odd while `write` runs. It starts from the next
/// odd value whatever the counter holds, so a writer that died between the two steps (process
/// killed, `panic = "abort"`) does not leave it odd for the life of the mapping, where every
/// reader would give up forever.
pub(super) fn seq_write(seq: &AtomicU32, write: impl FnOnce()) {
  let odd = seq.load(Ordering::Relaxed) | 1;
  seq.store(odd, Ordering::Relaxed);
  fence(Ordering::Release);
  write();
  seq.store(odd.wrapping_add(1), Ordering::Release);
}

fn write_text(slots: &[AtomicU16], text: &str) {
  let room = slots.len().saturating_sub(1);
  let units = text.encode_utf16().take(room).chain(std::iter::repeat(0));
  for (slot, unit) in slots.iter().zip(units) {
    slot.store(unit, Ordering::Relaxed);
  }
}

fn read_text(slots: &[AtomicU16]) -> String {
  let units: Vec<u16> = slots.iter().map(|s| s.load(Ordering::Relaxed)).take_while(|&u| u != 0).collect();
  String::from_utf16_lossy(&units)
}
