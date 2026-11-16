use super::*;

fn new_layout() -> Box<ShmLayout> {
  // Built on the heap: the 1 MB buffer would overflow the test thread's stack.
  let mut mem: Box<std::mem::MaybeUninit<ShmLayout>> = Box::new_uninit();
  // SAFETY: all-zero bytes are a valid ShmLayout (atomics, ints, floats); header is
  // then overwritten with defaults.
  unsafe {
    std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
    let mut mem = mem.assume_init();
    std::ptr::write(&mut mem.header, ShmHeader::default());
    mem
  }
}

/// A lazy band reader's result, comparable with `assert_eq!`.
fn collected((bands, preamp): (impl Iterator<Item = BiquadBand>, f32)) -> (Vec<BiquadBand>, f32) {
  (bands.collect(), preamp)
}

#[test]
fn test_ring_buffer_push_read() {
  let mut mem = new_layout();
  let mut ring = ShmRingBuffer::new(&mut mem);

  let frames = vec![
    StereoFrame { left: 0.2, right: 0.4 },
    StereoFrame { left: 0.6, right: 0.8 },
    StereoFrame { left: 1.0, right: 1.0 },
  ];
  ring.push_frames_timed(&frames, HNS_PER_SEC as u64);

  let mut out = [0.0; 3];
  ring.read_recent_frames(3, 0, &mut out);

  // mono = (l + r) * 0.5
  assert!((out[0] - 0.3).abs() < 1e-6);
  assert!((out[1] - 0.7).abs() < 1e-6);
  assert!((out[2] - 1.0).abs() < 1e-6);
}

#[test]
fn test_timed_push_maps_time_to_index_and_fills_gaps() {
  let mut mem = new_layout();
  let mut ring = ShmRingBuffer::new(&mut mem);
  let one = [StereoFrame { left: 1.0, right: 1.0 }; 480];

  // Packet at t = 1 s, 10 ms long.
  ring.push_frames_timed(&one, HNS_PER_SEC as u64);
  let idx_start = ring.header().ref_index_at(HNS_PER_SEC as u64);
  assert_eq!(idx_start, Some(0));

  // Next packet 100 ms later (90 ms of nothing played in between).
  ring.push_frames_timed(&one, HNS_PER_SEC as u64 + 1_000_000);
  let idx_second = ring.header().ref_index_at(HNS_PER_SEC as u64 + 1_000_000);
  assert_eq!(idx_second, Some(4800), "100 ms = 4800 frames after the first packet");

  let mut out = [0.0f32; 4];
  ring.read_frames_from(1000, &mut out);
  assert!(out.iter().all(|&s| s == 0.0), "gap must read as silence");
  ring.read_frames_from(4800, &mut out);
  assert!(out.iter().all(|&s| s == 1.0));
  // Future index reads as silence.
  ring.read_frames_from(10_000, &mut out);
  assert!(out.iter().all(|&s| s == 0.0));
}

#[test]
fn test_surround_frame_folds_down_to_stereo() {
  let mix = |frame: &[f32]| {
    let f = StereoFrame::from_channels(frame);
    [f.left, f.right]
  };
  let h = std::f32::consts::FRAC_1_SQRT_2;
  assert_eq!(mix(&[0.5]), [0.5, 0.5]);
  assert_eq!(mix(&[0.1, 0.2]), [0.1, 0.2]);
  // 5.1: the centre (a game's dialogue) and each surround reach the reference, the LFE does not.
  assert_eq!(mix(&[0.0, 0.0, 1.0, 9.0, 0.0, 0.0]), [h, h]);
  assert_eq!(mix(&[0.1, 0.2, 0.0, 0.0, 1.0, 0.0]), [0.1 + h, 0.2]);
  // 7.1: back and side pairs.
  assert_eq!(mix(&[0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0]), [0.0, 2.0 * h]);
  // Quad (no centre / LFE to tell apart): the front pair, as before.
  assert_eq!(mix(&[0.1, 0.2, 0.3, 0.4]), [0.1, 0.2]);
}

#[test]
fn test_ring_read_sanitizes_garbage() {
  let mut mem = new_layout();
  let mut ring = ShmRingBuffer::new(&mut mem);
  let frames = [
    StereoFrame { left: f32::INFINITY, right: 0.0 },
    StereoFrame { left: f32::NAN, right: 0.5 },
    StereoFrame { left: 1e30, right: 1e30 },
    StereoFrame { left: 0.5, right: 0.25 },
  ];
  ring.push_frames_timed(&frames, HNS_PER_SEC as u64);
  let mut out = [1.0f32; 4];
  ring.read_frames_from(0, &mut out);
  assert_eq!(out, [0.0, 0.0, REF_SAMPLE_MAX, 0.375]);
  let (mut left, mut right) = ([1.0f32; 5], [1.0f32; 5]);
  ring.read_stereo_frames_from(0, &mut left, &mut right);
  assert_eq!(left, [0.0, 0.0, REF_SAMPLE_MAX, 0.5, 0.0]);
  assert_eq!(right, [0.0, 0.0, REF_SAMPLE_MAX, 0.25, 0.0], "channels apart; not written yet = silence");
}

#[test]
fn another_layout_version_is_refused_and_named() {
  assert!(SHM_NAME.ends_with(&format!("_v{SHM_VERSION}")));
  assert!(APO_SHM_NAME.ends_with(&format!("_v{SHM_VERSION}")));
  let mut header = ShmHeader::default();
  assert_eq!(header.layout_mismatch(), None);
  header.version = SHM_VERSION - 1;
  let e = header.layout_mismatch().unwrap_or_default();
  assert!(e.contains(&(SHM_VERSION - 1).to_string()) && e.contains(&SHM_VERSION.to_string()), "{e}");
}

#[test]
fn test_mic_eq_roundtrip() {
  let header = ShmHeader::default();
  let bands = vec![
    BiquadBand { filter_type: FilterType::HighPass, frequency: 80.0, ..BiquadBand::default() },
    BiquadBand { enabled: false, gain_db: -3.0, q: 2.0, ..BiquadBand::default() },
  ];
  let v0 = header.mic_eq_version.load(Ordering::Relaxed);
  header.write_mic_eq(&bands);
  assert_ne!(header.mic_eq_version.load(Ordering::Relaxed), v0);
  assert_eq!(header.read_mic_eq().collect::<Vec<_>>(), bands);
}

#[test]
fn test_out_eq_roundtrip_and_mirroring() {
  let src = ShmHeader::default();
  let bands: Vec<BiquadBand> = (0..20)
    .map(|i| BiquadBand { frequency: 100.0 + i as f32 * 100.0, gain_db: 1.0, ..BiquadBand::default() })
    .collect();
  src.write_out_eq(&bands, -3.5);
  src.master_bypass.store(true, Ordering::Relaxed);
  assert_eq!(collected(src.read_out_eq()), (bands.clone(), -3.5));

  let dst = ShmHeader::default();
  dst.copy_controls_from(&src, &mut CopiedEqVersions::default());
  assert_eq!(collected(dst.read_out_eq()), (bands, -3.5));
  assert!(dst.master_bypass.load(Ordering::Relaxed));
}

#[test]
fn test_stream_eq_roundtrip_and_mirroring() {
  let (src, dst) = (ShmHeader::default(), ShmHeader::default());
  let band = BiquadBand { gain_db: -4.0, frequency: 2500.0, ..BiquadBand::default() };
  src.stream_eq[1].write(std::slice::from_ref(&band), -1.5);
  let mut seen = CopiedEqVersions::default();
  dst.copy_controls_from(&src, &mut seen);
  assert_eq!(collected(dst.stream_eq[1].read()), (vec![band], -1.5));
  assert_eq!(collected(dst.stream_eq[0].read()), (vec![], 0.0));
  // Unchanged source: nothing is written again (the APO keeps its coefficients).
  let v = dst.stream_eq[1].version.load(Ordering::Relaxed);
  dst.copy_controls_from(&src, &mut seen);
  assert_eq!(dst.stream_eq[1].version.load(Ordering::Relaxed), v);
}

#[test]
fn owner_watch_needs_a_changing_beat() {
  let h = ShmHeader::default();
  let mut watch = OwnerWatch::default();
  let t0 = Instant::now();
  h.owner_beat.store(41, Ordering::Relaxed);
  assert!(!watch.live(&h, t0), "a leftover beat is not an owner");
  assert!(!watch.live(&h, t0 + Duration::from_millis(100)));
  h.owner_beat.fetch_add(1, Ordering::Relaxed);
  assert!(watch.live(&h, t0 + Duration::from_millis(200)));
  assert!(watch.live(&h, t0 + Duration::from_millis(200) + OWNER_TIMEOUT / 2));
  assert!(!watch.live(&h, t0 + Duration::from_millis(200) + OWNER_TIMEOUT), "owner gone");
  h.owner_beat.fetch_add(1, Ordering::Relaxed);
  assert!(watch.live(&h, t0 + OWNER_TIMEOUT * 2), "owner back");
  // In this test, not another one: the last change seen is shared by the whole process.
  let mut new_stream = OwnerWatch::default();
  assert!(new_stream.live(&h, t0 + OWNER_TIMEOUT * 2), "a new watch after a live beat is live at once");
  let mut late_stream = OwnerWatch::default();
  let later = t0 + OWNER_TIMEOUT * 3 + Duration::from_millis(100);
  assert!(!late_stream.live(&h, later), "a beat that stopped is no owner for a new watch either");
}

#[test]
fn new_owner_copies_eqs_even_with_equal_versions() {
  let (src, dst) = (ShmHeader::default(), ShmHeader::default());
  let band = BiquadBand { gain_db: 3.0, ..BiquadBand::default() };
  src.write_mic_eq(std::slice::from_ref(&band));
  dst.write_mic_eq(&[]); // another daemon's EQ, same version number
  let apo_cached = dst.mic_eq_version.load(Ordering::Relaxed);
  dst.copy_controls_from(&src, &mut CopiedEqVersions::default());
  assert_eq!(dst.read_mic_eq().collect::<Vec<_>>(), vec![band]);
  // The APO re-reads only on a version it has not seen: it must change.
  assert_ne!(dst.mic_eq_version.load(Ordering::Relaxed), apo_cached);
}

#[test]
fn stream_eq_is_active_only_with_a_non_flat_bus_and_no_bypass() {
  let h = ShmHeader::default();
  assert!(!h.stream_eq_active());
  h.stream_eq[0].write(&[], -2.0);
  assert!(h.stream_eq_active(), "preamp alone changes the stream");
  h.master_bypass.store(true, Ordering::Relaxed);
  assert!(!h.stream_eq_active());
  h.master_bypass.store(false, Ordering::Relaxed);
  h.stream_eq[0].write(&[], 0.0);
  h.stream_eq[1].write(&[BiquadBand::default()], 0.0);
  assert!(h.stream_eq_active());
}

#[test]
fn a_seqlock_left_odd_by_a_dead_writer_recovers_on_the_next_write() {
  let h = ShmHeader::default();
  h.active_game_seq.store(7, Ordering::Relaxed); // writer killed mid-update
  assert_eq!(h.read_active_game(), None);
  let game = ActiveGame { profile: "p".into(), label: "l".into() };
  h.write_active_game(Some(&game));
  assert_eq!(h.read_active_game(), Some(game));
  assert_eq!(h.active_game_seq.load(Ordering::Relaxed) & 1, 0);
}

#[test]
fn test_sidetone_settings_roundtrip_and_mirroring() {
  let (daemon, apo) = (ShmHeader::default(), ShmHeader::default());
  assert!(!daemon.read_sidetone().enabled, "off until a pair switches it on");
  let wanted = SidetoneSettings {
    enabled: true,
    noise_mode: NoiseMode::RnNoise,
    volume: 1.5,
    gate_margin_db: 6.0,
    gate_range_db: 24.0,
  };
  daemon.write_sidetone(&wanted);
  daemon.sidetone_mic_hash.store(11, Ordering::Relaxed);
  daemon.sidetone_output_hash.store(22, Ordering::Relaxed);
  assert_eq!(daemon.read_sidetone(), wanted);
  let bands = crate::sidetone::default_bands();
  daemon.write_sidetone_eq(&bands);

  let mut seen = CopiedEqVersions::default();
  apo.copy_controls_from(&daemon, &mut seen);
  assert_eq!(apo.read_sidetone(), wanted);
  assert_eq!(apo.read_sidetone_eq().collect::<Vec<_>>(), bands);
  // Copied again only after the next edit: every copy makes the APO rebuild its filters.
  let version = apo.sidetone_eq_version.load(Ordering::Relaxed);
  apo.copy_controls_from(&daemon, &mut seen);
  assert_eq!(apo.sidetone_eq_version.load(Ordering::Relaxed), version);
  assert_eq!(
    (apo.sidetone_mic_hash.load(Ordering::Relaxed), apo.sidetone_output_hash.load(Ordering::Relaxed)),
    (11, 22)
  );

  // Telemetry goes the other way.
  apo.sidetone_writer_beat.store(5, Ordering::Relaxed);
  apo.sidetone_reader_beat.store(7, Ordering::Relaxed);
  apo.sidetone_age_us.store(9_000, Ordering::Relaxed);
  // Both APOs' parts come with the output telemetry (mirrored on every tick).
  daemon.copy_output_telemetry_from(&apo);
  assert_eq!(daemon.sidetone_writer_beat.load(Ordering::Relaxed), 5);
  assert_eq!(daemon.sidetone_reader_beat.load(Ordering::Relaxed), 7);
  assert_eq!(daemon.sidetone_age_us.load(Ordering::Relaxed), 9_000);

  // Garbage in the mapping (an older daemon, a torn write) stays inside the ranges.
  apo.sidetone_volume_permille.store(u32::MAX, Ordering::Relaxed);
  apo.sidetone_noise_mode.store(999, Ordering::Relaxed);
  let read = apo.read_sidetone();
  assert_eq!((read.volume, read.noise_mode), (crate::sidetone::VOLUME_MAX, NoiseMode::Expander));
}

#[test]
fn test_active_game_text_roundtrip() {
  let h = ShmHeader::default();
  assert_eq!(h.read_active_game(), None);
  let game =
    ActiveGame { profile: "The Division 2".into(), label: "Tom Clancy's The Division 2 (division2.exe)".into() };
  h.write_active_game(Some(&game));
  assert_eq!(h.read_active_game(), Some(game));
  let long = ActiveGame { profile: "\u{e9}".repeat(500), label: "x".into() };
  h.write_active_game(Some(&long));
  assert_eq!(h.read_active_game().map(|g| g.profile.chars().count()), Some(GAME_TEXT_LEN - 1));
  h.write_active_game(None);
  assert_eq!(h.read_active_game(), None);
}

#[test]
fn test_state_enums_roundtrip_and_unknown() {
  for s in [AecState::Disabled, AecState::NoReference, AecState::ReferenceSilent, AecState::Measuring, AecState::Locked]
  {
    assert_eq!(AecState::from_u32(s.to_u32()), s);
  }
  assert_eq!(AecState::from_u32(77), AecState::Unknown(77));
  assert_eq!(DenoiseModel::from_u32(1), DenoiseModel::DeepFilterNet);
  assert_eq!(DenoiseModel::from_u32(2), DenoiseModel::DpdfNet);
  for m in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    assert_eq!(DenoiseModel::from_u32(m.to_u32()), m);
  }
  assert_eq!(DenoiseModel::from_u32(9), DenoiseModel::Unknown(9));
  for l in [DenoiseLimit::None, DenoiseLimit::LoadFailed, DenoiseLimit::Overloaded] {
    assert_eq!(DenoiseLimit::from_u32(l.to_u32()), l);
  }
  assert_eq!(DenoiseLimit::from_u32(42), DenoiseLimit::Unknown(42));
}
