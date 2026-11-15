use super::*;
use windows::Win32::System::Com::CoTaskMemFree;

#[test]
fn capture_time_reads_the_v2_timestamp_only_with_its_signature() {
  let now = now_hns().unwrap();
  let mut v2 = APO_CONNECTION_PROPERTY_V2::default();
  v2.property.u32Signature = CONNECTION_V2_SIGNATURE;
  v2.property.u32ValidFrameCount = 480;
  v2.u64QPCTime = now - 1234;
  // SAFETY: a whole V2 property that outlives the call.
  let at = |v2: &APO_CONNECTION_PROPERTY_V2| unsafe { capture_time_hns(addr_of!(*v2).cast(), 48_000) };
  assert_eq!(at(&v2), Some(now - 1234));
  // A V1 signature: "now minus 10 ms", never the field behind the V1 struct.
  v2.property.u32Signature = 0;
  let t = at(&v2).unwrap();
  assert!(t >= now - 100_000 && t != now - 1234, "{t} vs now {now}");
}

#[test]
fn chain_writes_processed_mono_to_every_channel() {
  // In a user test process the Global mapping can't be created: the chain runs without it.
  let mut processor = MicProcessor::create(48_000, None, 1).unwrap();
  let input: Vec<f32> = (0..960).flat_map(|i| [((i as f32) * 0.05).sin() * 0.3, 0.9]).collect();

  // No daemon owns the mapping: the raw mic, untouched.
  let mut buf = input.clone();
  processor.process(&mut buf, 2, now_hns());
  assert_eq!(buf, input);

  processor.follow_owner = false;
  let mut buf = input;
  for _ in 0..20 {
    processor.process(&mut buf, 2, now_hns());
  }
  assert!(!processor.failed);
  assert!(buf.as_chunks::<2>().0.iter().all(|[l, r]| l == r && l.is_finite() && l.abs() <= 1.0));
}

#[test]
fn chain_runs_at_other_mic_rates_instead_of_passing_through() {
  // 44.1 kHz USB mics, 16/8 kHz Bluetooth headsets, 96 kHz interfaces.
  for rate in [8_000u32, 16_000, 44_100, 96_000] {
    let mut processor = MicProcessor::create(rate, None, 1).unwrap();
    processor.follow_owner = false;
    assert!(processor.bridge.is_some(), "{rate} Hz needs the resampling bridge");
    let frames = rate as usize / 100;
    for block in 0..50 {
      let mut buf: Vec<f32> = (0..frames)
        .flat_map(|i| {
          [(((block * frames + i) as f32) * 2.0 * std::f32::consts::PI * 300.0 / rate as f32).sin() * 0.3, 0.9]
        })
        .collect();
      processor.process(&mut buf, 2, now_hns());
      assert_eq!(buf.len(), frames * 2);
      assert!(buf.as_chunks::<2>().0.iter().all(|[l, r]| l == r && l.is_finite() && l.abs() <= 1.0), "{rate} Hz");
    }
    assert!(!processor.failed, "{rate} Hz");
  }
  assert!(MicProcessor::create(48_000, None, 1).unwrap().bridge.is_none());
  assert!(MicProcessor::create(1_000, None, 1).is_none(), "unsupported rates pass through");
}

#[test]
fn output_eq_follows_shared_memory_and_master_switch() {
  use gamersequalizer_common::profile::BiquadBand;
  let name = format!("Local\\GamersEqualizer_ApoTest_{}", std::process::id());
  let shm = SharedMemoryClient::create_named(&name, None).unwrap();
  let control = SharedMemoryClient::open_existing(&name).unwrap();
  let h = &control.layout().header;
  let mut processor = OutputProcessor::create(Some(shm), 48_000.0, 2).unwrap();
  let sine = |amp: f32| -> Vec<f32> {
    (0..9600).flat_map(|i| [((i as f32) * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * amp; 2]).collect()
  };
  let peak = |buf: &[f32]| buf[9600..].iter().fold(0.0f32, |m, s| m.max(s.abs()));

  // Nothing published: pass through, but the heartbeat counts.
  let mut buf = sine(0.1);
  processor.process(&mut buf, 2);
  assert_eq!(buf, sine(0.1));
  assert_eq!(h.out_apo_blocks.load(Ordering::Relaxed), 1);

  // +12 dB at 1 kHz with a -6 dB preamp, but no daemon beating: still untouched.
  h.write_out_eq(&[BiquadBand { gain_db: 12.0, ..BiquadBand::default() }], -6.0);
  let mut buf = sine(0.1);
  processor.process(&mut buf, 2);
  assert_eq!(buf, sine(0.1));

  // The owner beats: ~+6 dB on both channels.
  h.owner_beat.fetch_add(1, Ordering::Release);
  let mut buf = sine(0.1);
  processor.process(&mut buf, 2);
  let gain_db = 20.0 * (peak(&buf) / 0.1).log10();
  assert!((gain_db - 6.0).abs() < 0.3, "expected ~+6 dB, got {gain_db}");
  assert!(buf.as_chunks::<2>().0.iter().all(|[l, r]| l == r));

  // Master switch off: fades out over STREAM_FADE_SECONDS (no jump), then untouched.
  h.master_bypass.store(true, Ordering::Relaxed);
  let fade = 2 * (STREAM_FADE_SECONDS * 48_000.0) as usize + 2;
  let mut buf = sine(0.1);
  processor.process(&mut buf, 2);
  assert_ne!(buf[..fade], sine(0.1)[..fade]);
  assert_eq!(buf[fade..], sine(0.1)[fade..]);
  let mut buf = sine(0.1);
  processor.process(&mut buf, 2);
  assert_eq!(buf, sine(0.1));
}

#[test]
fn stream_eq_fades_in_on_pairing_and_out_on_master_switch() {
  use gamersequalizer_common::profile::BiquadBand;
  let name = format!(r"Local\GamersEqualizer_StreamTest_{}", std::process::id());
  let shm = SharedMemoryClient::create_named(&name, None).unwrap();
  let control = SharedMemoryClient::open_existing(&name).unwrap();
  let (h, table) = (&control.layout().header, &control.layout().streams);
  let mut p = StreamProcessor::create(shm, 48_000.0, 2, 0).unwrap();
  let (index, _) = p.slot.unwrap();
  // 10 ms blocks of a 1 kHz sine.
  let mut phase = 0usize;
  let mut block = |p: &mut StreamProcessor| -> (Vec<f32>, Vec<f32>) {
    let dry: Vec<f32> = (0..480)
      .flat_map(|i| [((phase + i) as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.1; 2])
      .collect();
    phase += 480;
    let mut out = dry.clone();
    p.process(&mut out, 2);
    (dry, out)
  };
  let peak = |b: &[f32]| b.iter().fold(0.0f32, |m, s| m.max(s.abs()));

  // Unpaired: untouched, but the level is published for pairing.
  let (dry, out) = block(&mut p);
  assert_eq!(out, dry);
  assert!(table.slots[index].frames.load(Ordering::Relaxed) >= 480);

  // Paired with chat (-12 dB at 1 kHz), owner beating: fades in over 20 ms, then -12 dB.
  h.owner_beat.fetch_add(1, Ordering::Release);
  h.stream_eq[0].write(&[BiquadBand { gain_db: -12.0, ..BiquadBand::default() }], 0.0);
  table.slots[index].bus.store(StreamBus::Chat.to_u32(), Ordering::Relaxed);
  let (_, first) = block(&mut p);
  assert!(peak(&first[..20]) > 0.0 && p.wet < 1.0, "still fading");
  for _ in 0..5 {
    block(&mut p);
  }
  let (_, settled) = block(&mut p);
  let gain_db = 20.0 * (peak(&settled) / 0.1).log10();
  assert!((gain_db + 12.0).abs() < 0.5, "expected ~-12 dB, got {gain_db}");

  // Master switch off: fades back to the untouched signal.
  h.master_bypass.store(true, Ordering::Relaxed);
  for _ in 0..4 {
    block(&mut p);
  }
  let (dry, out) = block(&mut p);
  assert_eq!(out, dry);
  assert!(!p.failed);
}

#[test]
fn call_timing_counts_late_calls_and_publishes_the_jitter_peak() {
  use std::time::Duration;
  let name = format!("Local\\GamersEqualizer_ApoTimingTest_{}", std::process::id());
  let shm = SharedMemoryClient::create_named(&name, None).unwrap();
  let h = &shm.layout().header;
  let mut timing = CallTiming::default();
  let t0 = Instant::now();
  let ms = |n: u64| t0 + Duration::from_millis(n);
  // 10 ms blocks, one call 2 ms late, then one 200 ms late (a stalled callback).
  let mut t = 0;
  for step in [10, 12, 10, 200].into_iter().chain(std::iter::repeat_n(10, 200)) {
    t += step;
    timing.note(ms(t), 480, 48_000, Some(h));
  }
  assert_eq!(h.mic_call_gaps.load(Ordering::Relaxed), 1, "only the stalled call is a gap");
  // The window with the stall was published, then a steady one replaced it.
  assert_eq!(h.mic_call_jitter_peak_us.load(Ordering::Relaxed), 0);
  let mut timing = CallTiming::default();
  for i in 0..=CALL_TIMING_WINDOW_CALLS as u64 {
    timing.note(ms(i * 10 + if i == 5 { 3 } else { 0 }), 480, 48_000, Some(h));
  }
  assert_eq!(h.mic_call_jitter_peak_us.load(Ordering::Relaxed), 3_000);
}

#[test]
fn mic_clock_filters_jitter_and_resyncs_after_a_gap() {
  let mut clock = MicClock::new(48_000);
  let start = 1_000_000_000u64;
  let mut prev = clock.next(Some(start), 480).unwrap();
  for i in 1..500u64 {
    // 10 ms blocks, raw times jittering by +-3 ms.
    let jitter = [30_000i64, -30_000, 15_000, -15_000][(i % 4) as usize];
    let raw = (start as i64 + i as i64 * 100_000 + jitter) as u64;
    let t = clock.next(Some(raw), 480).unwrap();
    let step = t as i64 - prev as i64;
    assert!((step - 100_000).abs() < 500, "block {i}: step {step}");
    prev = t;
  }
  // A 2 s gap re-anchors immediately.
  let after_gap = prev + 20_000_000;
  assert_eq!(clock.next(Some(after_gap), 480), Some(after_gap));
}

#[test]
fn process_separate_and_in_place_buffers() {
  let mut src = vec![0.1f32, -0.2, 0.3, -0.4];
  let mut dst = vec![9.0f32; 4];
  let input = connection(&mut src, 2, BUFFER_VALID.0);
  let mut output = connection(&mut dst, 0, BUFFER_VALID.0);
  // SAFETY: the connections describe the vectors above with their frame counts; both outlive the call.
  unsafe { process_connection(&input, &mut output, 2, double) };
  assert_eq!(output.u32ValidFrameCount, 2);
  assert_eq!(dst, vec![0.2, -0.4, 0.6, -0.8]);

  let mut buf = vec![0.25f32, -0.25];
  let input = connection(&mut buf, 1, BUFFER_VALID.0);
  let mut output = input;
  // SAFETY: the connections describe the vectors above with their frame counts; both outlive the call.
  unsafe { process_connection(&input, &mut output, 2, double) };
  assert_eq!(buf, vec![0.5, -0.5]);
}

#[test]
fn silent_input_zeroes_separate_output() {
  let mut src = vec![0.5f32; 4];
  let mut dst = vec![0.7f32; 4];
  let input = connection(&mut src, 2, BUFFER_SILENT.0);
  let mut output = connection(&mut dst, 0, BUFFER_VALID.0);
  // SAFETY: the connections describe the vectors above with their frame counts; both outlive the call.
  unsafe { process_connection(&input, &mut output, 2, double) };
  assert_eq!(output.u32BufferFlags, BUFFER_SILENT);
  assert_eq!(dst, vec![0.0; 4]);
}

#[test]
fn output_and_stream_classes_register_their_own_clsid() {
  for clsid in [CLSID_OUTPUT_EQ_APO, CLSID_STREAM_APO] {
    let mut raw = null_mut();
    // SAFETY: valid in/out pointers; on success `raw` holds a factory reference that `from_raw`
    // takes over; the properties block is ours to read and free.
    unsafe {
      assert!(DllGetClassObject(&clsid, &IClassFactory::IID, &mut raw).is_ok());
      let factory = IClassFactory::from_raw(raw);
      let apo: IAudioProcessingObject = factory.CreateInstance(None).unwrap();
      let props = apo.GetRegistrationProperties().unwrap();
      assert_eq!((*props).clsid, clsid);
      CoTaskMemFree(Some(props as *const c_void));
    }
  }
}

#[test]
fn only_one_mic_chain_publishes_and_the_next_takes_over() {
  // Other tests create mic chains too (shared statics): only what holds regardless.
  let (mut first, mut second) = (MicChainCount::new(), MicChainCount::new());
  assert_ne!(first.id, second.id);
  assert!(!(first.publishes() && second.publishes()), "two publishers at once");
  if first.publishes() {
    // `first` goes quiet (failed, or its stream paused): after enough of its own blocks
    // without a beat `second` takes over, unless another test's chain beats in between.
    let quiet = first.id;
    let took_over = (0..PUBLISHER_QUIET_BLOCKS + 2).any(|_| second.publishes());
    assert!(took_over || MIC_PUBLISHER.load(Ordering::Relaxed) != quiet);
    drop(first);
    assert_ne!(MIC_PUBLISHER.load(Ordering::Relaxed), quiet, "a closed chain is never the publisher");
  }
}

#[test]
fn processing_mode_is_read_from_init_block_and_short_blocks_are_ignored() {
  let mode = GUID::from_u128(0x98951333_b9cd_48b1_a0a3_ff40682d73f7);
  // SAFETY: all zero is a valid block: integers, GUIDs and null (None) COM pointers.
  let mut init: APOInitSystemEffects2 = unsafe { std::mem::zeroed() };
  init.APOInit.cbSize = size_of::<APOInitSystemEffects2>() as u32;
  init.AudioProcessingMode = mode;
  let data = &init as *const APOInitSystemEffects2 as *const u8;
  let size = size_of::<APOInitSystemEffects2>() as u32;
  // SAFETY: `data` points to `size` readable bytes of `init` (20 claims fewer), or is null.
  unsafe {
    assert_eq!(processing_mode(size, data), Some(mode.to_u128()));
    assert_eq!(endpoint_guid_from_init(size, data, None), None);
    assert_eq!(processing_mode(20, data), None);
    assert_eq!(endpoint_guid_from_init(20, data, None), None);
    assert_eq!(processing_mode(size, std::ptr::null()), None);
  }
  assert_eq!(
    normalize_endpoint_guid("{0.0.0.00000000}.{42B05AAC-7B95-4CC8-8481-B940A20FE8BD}").as_deref(),
    Some("{42b05aac-7b95-4cc8-8481-b940a20fe8bd}")
  );
  assert_eq!(
    normalize_endpoint_guid(" {CF0A2270-1885-4DD6-82A5-F636E9EFADD5} ").as_deref(),
    Some("{cf0a2270-1885-4dd6-82a5-f636e9efadd5}")
  );
  assert_eq!(normalize_endpoint_guid("garbage"), None);
}

#[test]
fn class_factory_exposes_all_apo_interfaces() {
  // SAFETY: COM calls on live interfaces we hold.
  let apo: IAudioProcessingObject = unsafe { factory().CreateInstance(None) }.unwrap();
  apo.cast::<IAudioProcessingObjectRT>().unwrap();
  apo.cast::<IAudioProcessingObjectConfiguration>().unwrap();
  apo.cast::<IAudioSystemEffects>().unwrap();
  assert!(apo.cast::<IClassFactory>().is_err());

  // SAFETY: a live interface; the properties block holds the 4 IIDs it declares and is ours to free.
  unsafe {
    let props = apo.GetRegistrationProperties().unwrap();
    assert_eq!((*props).clsid, CLSID_MIC_APO);
    assert_eq!((*props).u32NumAPOInterfaces, 4);
    let list = addr_of_mut!((*props).iidAPOInterfaceList) as *const GUID;
    assert_eq!(*list.add(3), IAudioSystemEffects::IID);
    CoTaskMemFree(Some(props as *const c_void));
  }
}

/// Regression: audiodg creates APOs aggregated; refusing that disabled the microphone.
#[test]
fn aggregation_delegates_to_outer() {
  // Any COM object works as the outer; a class factory is the one we have at hand.
  let outer: IUnknown = factory().cast().unwrap();
  // SAFETY: a COM call on live interfaces we hold.
  let inner: IUnknown = unsafe { factory().CreateInstance(&outer) }.unwrap();
  assert_ne!(inner.as_raw(), outer.as_raw());

  let apo: IAudioProcessingObject = inner.cast().unwrap();
  // QI through an APO interface goes to the outer object: it finds the outer's interface...
  assert!(apo.cast::<IClassFactory>().is_ok());
  // ...and asking aggregated for anything but IUnknown is refused.
  // SAFETY: as above.
  let refused: Result<IAudioProcessingObject> = unsafe { factory().CreateInstance(&outer) };
  assert_eq!(refused.unwrap_err().code(), CLASS_E_NOAGGREGATION);

  let rt: IAudioProcessingObjectRT = inner.cast().unwrap();
  // SAFETY: as above.
  assert_eq!(unsafe { rt.CalcOutputFrames(480) }, 480);
}

#[test]
fn wrong_clsid_and_bad_calls_fail_without_crashing() {
  let mut raw = null_mut();
  let other = GUID::from_u128(1);
  // SAFETY: valid in/out pointers, then COM calls on live interfaces we hold. The null connection
  // arrays are the bad call under test: APOProcess must check them, not read them.
  unsafe {
    assert_eq!(DllGetClassObject(&other, &IClassFactory::IID, &mut raw), CLASS_E_CLASSNOTAVAILABLE);
    assert!(raw.is_null());

    let apo: IAudioProcessingObject = factory().CreateInstance(None).unwrap();
    let config = apo.cast::<IAudioProcessingObjectConfiguration>().unwrap();
    assert_eq!(config.UnlockForProcess().unwrap_err().code(), APOERR_ALREADY_UNLOCKED);
    let rt = apo.cast::<IAudioProcessingObjectRT>().unwrap();
    rt.APOProcess(1, std::ptr::null(), 1, null_mut());
    assert_eq!(apo.GetInputChannelCount().unwrap(), 1);
  }
}

fn double(buf: &mut [f32]) {
  buf.iter_mut().for_each(|s| *s *= 2.0);
}

fn factory() -> IClassFactory {
  let mut raw = null_mut();
  // SAFETY: valid in/out pointers; on success `raw` holds a factory reference that `from_raw` takes over.
  unsafe {
    assert!(DllGetClassObject(&CLSID_MIC_APO, &IClassFactory::IID, &mut raw).is_ok());
    IClassFactory::from_raw(raw)
  }
}

fn connection(buf: &mut [f32], frames: u32, flags: i32) -> APO_CONNECTION_PROPERTY {
  APO_CONNECTION_PROPERTY {
    pBuffer: buf.as_mut_ptr() as usize,
    u32ValidFrameCount: frames,
    u32BufferFlags: windows::Win32::Media::Audio::Apo::APO_BUFFER_FLAGS(flags),
    u32Signature: 0,
  }
}

#[test]
fn sidetone_plays_on_the_pair_output_only() {
  use gamersequalizer_common::sidetone::SidetoneSettings;
  const MIC: u32 = 11;
  const OUT: u32 = 22;
  let name = format!(r"Local\GamersEqualizer_SidetoneTest_{}", std::process::id());
  let shm = SharedMemoryClient::create_named(&name, None).unwrap();
  let control = SharedMemoryClient::open_existing(&name).unwrap();
  let layout = control.layout();
  let h = &layout.header;
  h.write_sidetone(&SidetoneSettings { enabled: true, noise_mode: NoiseMode::Off, ..Default::default() });
  h.sidetone_mic_hash.store(MIC, Ordering::Relaxed);
  h.sidetone_output_hash.store(OUT, Ordering::Relaxed);

  let mut tap = SidetoneTap::new(48_000, 1);
  tap.endpoint_hash = MIC;
  let mut output = OutputProcessor::create(Some(shm), 48_000.0, 2).unwrap();
  output.sidetone_endpoint = OUT;
  // 10 ms blocks of a 1 kHz tone at 0.25 on the mic, silence on the output.
  let run = |tap: &mut SidetoneTap, output: &mut OutputProcessor, blocks: usize| -> f32 {
    let mut peak = 0.0f32;
    for k in 0..blocks {
      h.owner_beat.fetch_add(1, Ordering::Release);
      let mic: Vec<f32> = (0..480)
        .flat_map(|i| [((k * 480 + i) as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin() * 0.25; 2])
        .collect();
      let before = mic.clone();
      tap.process(layout, &mic, 2, None, 0.0);
      assert_eq!(mic, before, "the tap only reads the microphone buffer");
      let mut out = vec![0.0f32; 960];
      output.process(&mut out, 2);
      assert!(out.as_chunks::<2>().0.iter().all(|[l, r]| l == r && l.is_finite()));
      if k >= blocks / 2 {
        peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
      }
    }
    peak
  };

  let heard = run(&mut tap, &mut output, 100);
  assert!((heard - 0.25).abs() < 0.02, "sidetone at 100 %, no EQ, leveler at 0 dB: {heard}");
  assert_eq!(h.sidetone_underruns.load(Ordering::Relaxed), 0);
  assert!(h.sidetone_writer_beat.load(Ordering::Relaxed) > 0 && h.sidetone_reader_beat.load(Ordering::Relaxed) > 0);

  // Another output (speakers), and an output that does not know what it is: silence.
  for endpoint in [OUT + 1, 0] {
    output.sidetone_endpoint = endpoint;
    assert_eq!(run(&mut tap, &mut output, 20), 0.0, "endpoint {endpoint}");
  }
  output.sidetone_endpoint = OUT;
  assert!(run(&mut tap, &mut output, 100) > 0.2, "back on the pair's output");

  // The microphone of another pair stops feeding the ring.
  tap.endpoint_hash = MIC + 1;
  assert_eq!(run(&mut tap, &mut output, 100), 0.0);

  // Muted microphone: silence, no click (the level ramps down first).
  tap.endpoint_hash = MIC;
  assert!(run(&mut tap, &mut output, 100) > 0.2);
  h.mic_muted.store(true, Ordering::Relaxed);
  assert_eq!(run(&mut tap, &mut output, 100), 0.0);
}

#[test]
fn endpoint_device_id_rejects_blocks_it_does_not_know() {
  // SAFETY: null, and a buffer of the stated size.
  unsafe {
    assert_eq!(endpoint_device_id(0, std::ptr::null()), None);
    let short = [0u8; 8];
    assert_eq!(endpoint_device_id(short.len() as u32, short.as_ptr()), None);
    // Right size, but cbSize says another layout (e.g. APOInitSystemEffects3). The collection
    // slot holds a pointer that must never be called: the guard has to stop before it.
    let mut init: APOInitSystemEffects2 = std::mem::zeroed();
    init.APOInit.cbSize = size_of::<APOInitSystemEffects2>() as u32 + 8;
    let poison = std::ptr::dangling_mut::<c_void>();
    addr_of_mut!(init.pDeviceCollection).cast::<*mut c_void>().write_unaligned(poison);
    let data = &init as *const _ as *const u8;
    let size = size_of::<APOInitSystemEffects2>() as u32;
    assert_eq!(endpoint_device_id(size, data), None);
    assert_eq!(endpoint_guid_from_init(size, data, None), None, "no endpoint properties, no device id");
    addr_of_mut!(init.pDeviceCollection).cast::<*mut c_void>().write_unaligned(std::ptr::null_mut());
    // Known layout, no device collection.
    init.APOInit.cbSize = size_of::<APOInitSystemEffects2>() as u32;
    assert_eq!(endpoint_device_id(size_of::<APOInitSystemEffects2>() as u32, &init as *const _ as *const u8), None);
  }
}
