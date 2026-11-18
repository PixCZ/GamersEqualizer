//! The `IAudioProcessingObject*` methods the engine calls (`apo_*`, `rt_*`, `cfg_*`), the state they
//! share and the format checks.

use super::*;

/// Global mapping DACL: full access for SYSTEM, LOCAL SERVICE (audiodg) and Administrators;
/// interactively logged-on users (the daemon) only map read/write (`0x6` =
/// SECTION_MAP_WRITE | SECTION_MAP_READ), so they cannot rewrite the DACL and lock audiodg out.
/// Known limit: every interactive user on the PC still shares one mapping (not supported for
/// several users at once, see README "Known limits"); per-user mappings would be the upgrade.
pub(super) const APO_SHM_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;0x6;;;IU)";

static APO_INSTANCE_IDS: AtomicU32 = AtomicU32::new(1);

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

const MAX_CHANNELS: u32 = 32;

/// Which COM class an object was created as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApoKind {
  Mic,
  Output,
  Stream,
}

/// Processing state shared by all interfaces; atomics because the engine calls
/// configuration and real-time methods from different threads.
pub(super) struct ApoState {
  pub(super) kind: ApoKind,
  pub(super) instance_id: u32,
  /// Endpoint GUID (`{...}`, lowercased) from `Initialize`, when Windows supplies it.
  pub(super) endpoint_tag: OnceLock<String>,
  locked: AtomicBool,
  channels: AtomicU32,
  /// `device_hash` of the endpoint from Initialize; 0 = the engine did not say.
  endpoint: AtomicU32,
  /// Frames per second of the locked format (0 before LockForProcess).
  rate: AtomicU32,
  /// `AudioProcessingMode` from Initialize (high, low half of the GUID); 0 = not reported.
  /// Written in Initialize, read in LockForProcess: engine-serialized calls, no tearing.
  mode: (AtomicU64, AtomicU64),
  /// Built in LockForProcess, dropped in UnlockForProcess; RT uses `try_lock` only.
  processor: Mutex<Option<Processor>>,
}

enum Processor {
  /// Boxed: the mic chain is kilobytes, the stream processor about 13 KB (its voice detector,
  /// see `stream.rs`), the output processor hundreds of bytes.
  Mic(Box<MicProcessor>),
  Output(Box<OutputProcessor>),
  Stream(Box<StreamProcessor>),
}

pub(super) unsafe extern "system" fn apo_reset(_this: *mut c_void) -> HRESULT {
  S_OK
}

pub(super) unsafe extern "system" fn apo_get_latency(_this: *mut c_void, latency: *mut i64) -> HRESULT {
  // SAFETY: `latency` is null-checked; the engine owns it for the call.
  unsafe {
    if latency.is_null() {
      return E_POINTER;
    }
    *latency = 0;
    S_OK
  }
}

pub(super) unsafe extern "system" fn apo_get_registration_properties(
  this: *mut c_void,
  out: *mut *mut APO_REG_PROPERTIES,
) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_APO` of a live `ApoObject` and holds a
  // reference for the call (`object`); `out` is null-checked.
  unsafe {
    if out.is_null() {
      return E_POINTER;
    }
    match registration_properties(state::<OFF_APO>(this).kind) {
      Ok(props) => {
        *out = props;
        S_OK
      }
      Err(err) => {
        *out = null_mut();
        err.code()
      }
    }
  }
}

pub(super) unsafe extern "system" fn apo_initialize(this: *mut c_void, size: u32, data: *const u8) -> HRESULT {
  guarded("Initialize", E_UNEXPECTED, || {
    // SAFETY: COM calls this slot only through our interface pointer at `OFF_APO` of a live `ApoObject` and holds a
    // reference for the call (`object`); `data` is the engine's init block of `size` bytes (or null), which each helper
    // checks before reading a version's fields.
    unsafe {
      let st = state::<OFF_APO>(this);
      if st.locked.load(Ordering::Acquire) {
        return APOERR_APO_LOCKED;
      }
      let device_id = endpoint_device_id(size, data);
      if let Some(guid) = endpoint_guid_from_init(size, data, device_id.as_deref()) {
        let _ = st.endpoint_tag.set(guid);
      }
      let mode = processing_mode(size, data);
      if let Some(mode) = mode {
        st.mode.0.store((mode >> 64) as u64, Ordering::Relaxed);
        st.mode.1.store(mode as u64, Ordering::Relaxed);
      }
      // The block size tells which APOInitSystemEffects version Windows passed (the mode is in v2+).
      let mode = mode.map_or_else(|| "not given".to_string(), |m| format!("{:?}", GUID::from_u128(m)));
      note_for(st, Level::Info, format!("initialized ({size}-byte init block, mode {mode})"));
      if let Some(hash) = device_id.as_deref().map(device_hash) {
        st.endpoint.store(hash, Ordering::Relaxed);
      }
      S_OK
    }
  })
}

pub(super) unsafe extern "system" fn apo_is_input_format_supported(
  this: *mut c_void,
  opposite: *mut c_void,
  requested: *mut c_void,
  supported: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_APO` of a live `ApoObject` and holds a
  // reference for the call (`object`); the media types are the engine's (contract of `is_format_supported`).
  unsafe { is_format_supported(this, "input", opposite, requested, supported) }
}

pub(super) unsafe extern "system" fn apo_is_output_format_supported(
  this: *mut c_void,
  opposite: *mut c_void,
  requested: *mut c_void,
  supported: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: as `apo_is_input_format_supported`.
  unsafe { is_format_supported(this, "output", opposite, requested, supported) }
}

/// # Safety
/// `this` is our IAudioProcessingObject pointer; the rest are the engine's COM pointers.
unsafe fn is_format_supported(
  this: *mut c_void,
  direction: &str,
  opposite: *mut c_void,
  requested: *mut c_void,
  supported: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: the contract above; `supported` is null-checked, the media types are borrowed for the call and the
  // accepted one is handed out with its own reference (`into_raw`).
  unsafe {
    if supported.is_null() {
      return E_POINTER;
    }
    *supported = null_mut();
    let opposite = IAudioMediaType::from_raw_borrowed(&opposite);
    let requested = IAudioMediaType::from_raw_borrowed(&requested);
    let result = check_format(opposite, requested);
    // A refused format drops the effect from the graph before it ever starts a stream.
    let describe = |media: Option<&IAudioMediaType>| {
      media.and_then(uncompressed_format).map_or_else(|| "none".to_string(), |f| describe_format(&f))
    };
    let (level, verdict) = match &result {
      Ok(_) => (Level::Info, "accepted".to_string()),
      Err(err) => (Level::Warning, format!("refused ({:?})", err.code())),
    };
    let st = state::<OFF_APO>(this);
    let (requested_text, opposite_text) = (describe(requested), describe(opposite));
    note_for(st, level, format!("{direction} format {requested_text} (other side {opposite_text}): {verdict}"));
    match result {
      Ok(format) => {
        *supported = format.into_raw();
        S_OK
      }
      Err(err) => err.code(),
    }
  }
}

pub(super) unsafe extern "system" fn apo_get_input_channel_count(this: *mut c_void, count: *mut u32) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_APO` of a live `ApoObject` and holds a
  // reference for the call (`object`); `count` is null-checked.
  unsafe {
    if count.is_null() {
      return E_POINTER;
    }
    *count = state::<OFF_APO>(this).channels.load(Ordering::Acquire);
    S_OK
  }
}

pub(super) unsafe extern "system" fn rt_apo_process(
  this: *mut c_void,
  num_inputs: u32,
  inputs: *const *const APO_CONNECTION_PROPERTY,
  num_outputs: u32,
  outputs: *mut *mut APO_CONNECTION_PROPERTY,
) {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_RT` of a live `ApoObject` and holds a
  // reference for the call (`object`). The engine passes one input and one output connection (checked, as are the
  // pointers) that stay valid for this call, with buffers as `process_connection` needs.
  unsafe {
    if num_inputs != 1 || num_outputs != 1 || inputs.is_null() || outputs.is_null() {
      return;
    }
    let st = state::<OFF_RT>(this);
    let channels = st.channels.load(Ordering::Relaxed) as usize;
    let input = *inputs;
    let output = *outputs;
    if input.is_null() || output.is_null() {
      return;
    }
    // The mic chain lines its audio up with the reference by time; the output writes the reference.
    let capture_time =
      (st.kind == ApoKind::Mic).then(|| capture_time_hns(input, st.rate.load(Ordering::Relaxed))).flatten();
    process_connection(&*input, &mut *output, channels, |buf| {
      // Never block the audio thread: if configuration holds the lock, pass this block through.
      if let Ok(mut guard) = st.processor.try_lock()
        && let Some(processor) = guard.as_mut()
      {
        // The output's block time is read only while this endpoint is the reference output.
        let render_time = match &*processor {
          Processor::Output(p) if p.wants_block_time() => render_time_hns(input),
          _ => None,
        };
        processor.process(buf, channels, capture_time, render_time);
      }
    });
    // A silent output block is not processed, but the reference must count it.
    if st.kind == ApoKind::Output
      && (*input).u32BufferFlags == BUFFER_SILENT
      && let Ok(mut guard) = st.processor.try_lock()
      && let Some(Processor::Output(processor)) = guard.as_mut()
    {
      let render_time = if processor.wants_block_time() { render_time_hns(input) } else { None };
      processor.process_silence((*input).u32ValidFrameCount as usize, channels, render_time);
    }
  }
}

pub(super) unsafe extern "system" fn rt_calc_frames(_this: *mut c_void, frames: u32) -> u32 {
  frames
}

pub(super) unsafe extern "system" fn cfg_lock_for_process(
  this: *mut c_void,
  num_inputs: u32,
  inputs: *const *const APO_CONNECTION_DESCRIPTOR,
  num_outputs: u32,
  outputs: *const *const APO_CONNECTION_DESCRIPTOR,
) -> HRESULT {
  guarded("LockForProcess", E_UNEXPECTED, || {
    // SAFETY: COM calls this slot only through our interface pointer at `OFF_CFG` of a live `ApoObject` and holds a
    // reference for the call (`object`); the one input descriptor is null-checked, its `pFormat` is the engine's media
    // type or null.
    unsafe {
      let st = state::<OFF_CFG>(this);
      if st.locked.load(Ordering::Acquire) {
        return APOERR_APO_LOCKED;
      }
      if num_inputs != 1 || num_outputs != 1 || inputs.is_null() || outputs.is_null() {
        note_for(st, Level::Warning, format!("refused a stream with {num_inputs} inputs and {num_outputs} outputs"));
        return APOERR_NUM_CONNECTIONS_INVALID;
      }
      let input = *inputs;
      if input.is_null() {
        return E_POINTER;
      }
      let offered = (*input).pFormat.as_ref().and_then(uncompressed_format);
      let Some(format) = offered.filter(is_supported_format) else {
        let what = offered.as_ref().map_or_else(|| "unreadable".to_string(), describe_format);
        note_for(
          st,
          Level::Warning,
          format!("stream format not supported ({what}); Windows runs the stream without it"),
        );
        return APOERR_FORMAT_NOT_SUPPORTED;
      };

      st.channels.store(format.dwSamplesPerFrame, Ordering::Release);
      let rate = format.fFramesPerSecond;
      st.rate.store(rate as u32, Ordering::Release);
      let mode = (u128::from(st.mode.0.load(Ordering::Relaxed)) << 64) | u128::from(st.mode.1.load(Ordering::Relaxed));
      let tag = st.endpoint_tag.get().cloned();
      let endpoint = st.endpoint.load(Ordering::Relaxed);
      let processor = match st.kind {
        ApoKind::Mic => MicProcessor::create(rate as u32, tag.clone(), st.instance_id).map(|mut p| {
          p.sidetone.endpoint_hash = endpoint;
          Processor::Mic(p)
        }),
        ApoKind::Output => OutputProcessor::create(shared_memory(st), rate, format.dwSamplesPerFrame, st.instance_id)
          .map(|mut p| {
            p.endpoint = endpoint;
            Processor::Output(Box::new(p))
          }),
        ApoKind::Stream => shared_memory(st)
          .and_then(|shm| StreamProcessor::create(shm, rate, format.dwSamplesPerFrame, mode))
          .map(|p| Processor::Stream(Box::new(p))),
      };
      let level = if processor.is_some() { Level::Info } else { Level::Warning };
      let outcome = match &processor {
        Some(Processor::Mic(_)) => format!("started, mic chains running here: {}", MIC_CHAINS.load(Ordering::Relaxed)),
        Some(_) => "started".to_string(),
        None => "no chain, passes audio through".to_string(),
      };
      let msg = format!("stream {}, mode {:?}: {outcome}", describe_format(&format), GUID::from_u128(mode));
      note_for(st, level, msg);
      if let Some(Processor::Output(p)) = &processor {
        let no_reference = "this output never feeds the echo canceller's speaker reference";
        if p.endpoint == 0 {
          let why = "Windows did not say which device this output is: it plays no sidetone and";
          note_for(st, Level::Warning, format!("{why} {no_reference}"));
        }
        if let Err(e) = &p.ref_feed {
          note_for(st, Level::Warning, format!("{e}: {no_reference}"));
        }
      }
      if let Ok(mut slot) = st.processor.lock() {
        // Replacing drops a previous processor: keep a destructor panic inside this DLL.
        let old = std::mem::replace(&mut *slot, processor);
        let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
      }
      st.locked.store(true, Ordering::Release);
      S_OK
    }
  })
}

pub(super) unsafe extern "system" fn cfg_unlock_for_process(this: *mut c_void) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_CFG` of a live `ApoObject` and holds a
  // reference for the call (`object`).
  unsafe {
    let st = state::<OFF_CFG>(this);
    if !st.locked.swap(false, Ordering::AcqRel) {
      return APOERR_ALREADY_UNLOCKED;
    }
    if let Ok(mut slot) = st.processor.lock() {
      match slot.as_ref() {
        Some(Processor::Mic(mic)) => mic.note_stop(),
        Some(Processor::Output(out)) => {
          if let Some((level, text)) = out.reference_stop_note() {
            note_for(st, level, text);
          }
        }
        // Off the audio thread, like the others: the failure itself only set a flag.
        Some(Processor::Stream(stream)) if stream.failed => {
          let text =
            "processing panicked: passed audio through (no stream EQ, no Automatic ChatMix) until the stream stopped";
          note_for(st, Level::Warning, text.to_string());
        }
        _ => {}
      }
      // Dropping may run arbitrary DSP destructors: keep a panic inside this DLL.
      let old = slot.take();
      let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
    }
    S_OK
  }
}

/// The Global mapping, created or opened. Without it the effect passes audio through and the
/// app never connects, so a failure is noted in the Application log.
fn shared_memory(st: &ApoState) -> Option<SharedMemoryClient> {
  shared_memory_tagged(st.kind, st.endpoint_tag.get().map(String::as_str), st.instance_id)
}

pub(super) fn shared_memory_tagged(
  kind: ApoKind,
  endpoint_tag: Option<&str>,
  instance_id: u32,
) -> Option<SharedMemoryClient> {
  match SharedMemoryClient::create_named(APO_SHM_NAME, Some(APO_SHM_SDDL)) {
    Ok(shm) => Some(shm),
    Err(e) => {
      note_tagged(
        kind,
        endpoint_tag,
        instance_id,
        Level::Warning,
        format!("shared memory {APO_SHM_NAME} unavailable ({e})"),
      );
      None
    }
  }
}

/// "48000 Hz, 2 ch, float 4 bytes" for the Application log notes.
fn describe_format(format: &UNCOMPRESSEDAUDIOFORMAT) -> String {
  let float = if format.guidFormatType == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT { "float" } else { "not float" };
  format!(
    "{} Hz, {} ch, {float} {} bytes",
    format.fFramesPerSecond, format.dwSamplesPerFrame, format.dwBytesPerSampleContainer
  )
}

impl ApoKind {
  pub(super) fn from_clsid(clsid: &GUID) -> Option<Self> {
    match *clsid {
      CLSID_MIC_APO => Some(ApoKind::Mic),
      CLSID_OUTPUT_EQ_APO => Some(ApoKind::Output),
      CLSID_STREAM_APO => Some(ApoKind::Stream),
      _ => None,
    }
  }

  pub(super) fn clsid(self) -> GUID {
    match self {
      ApoKind::Mic => CLSID_MIC_APO,
      ApoKind::Output => CLSID_OUTPUT_EQ_APO,
      ApoKind::Stream => CLSID_STREAM_APO,
    }
  }

  pub(super) fn friendly_name(self) -> &'static str {
    match self {
      ApoKind::Mic => "GamersEqualizer Mic Chain",
      ApoKind::Output => "GamersEqualizer Output EQ",
      ApoKind::Stream => "GamersEqualizer Stream EQ",
    }
  }
}

impl ApoState {
  pub(super) fn new(kind: ApoKind) -> Self {
    Self {
      kind,
      instance_id: APO_INSTANCE_IDS.fetch_add(1, Ordering::Relaxed),
      endpoint_tag: OnceLock::new(),
      locked: AtomicBool::new(false),
      channels: AtomicU32::new(1),
      endpoint: AtomicU32::new(0),
      rate: AtomicU32::new(0),
      mode: (AtomicU64::new(0), AtomicU64::new(0)),
      processor: Mutex::new(None),
    }
  }
}

impl Processor {
  /// `capture_time`: the mic block's recording time; `render_time`: the output block's play time.
  fn process(&mut self, buf: &mut [f32], channels: usize, capture_time: Option<u64>, render_time: Option<(u64, bool)>) {
    match self {
      Processor::Mic(p) => p.process(buf, channels, capture_time),
      Processor::Output(p) => p.process(buf, channels, render_time),
      Processor::Stream(p) => p.process(buf, channels),
    }
  }
}

fn check_format(opposite: Option<&IAudioMediaType>, requested: Option<&IAudioMediaType>) -> Result<IAudioMediaType> {
  let requested = requested.ok_or(E_POINTER)?;
  let wanted = uncompressed_format(requested).filter(is_supported_format).ok_or(APOERR_FORMAT_NOT_SUPPORTED)?;
  if let Some(other) = opposite.and_then(uncompressed_format) {
    // In-place processing: rate and channel layout cannot change between input and output.
    if other.dwSamplesPerFrame != wanted.dwSamplesPerFrame || other.fFramesPerSecond != wanted.fFramesPerSecond {
      return Err(APOERR_FORMAT_NOT_SUPPORTED.into());
    }
  }
  Ok(requested.clone())
}

fn uncompressed_format(media: &IAudioMediaType) -> Option<UNCOMPRESSEDAUDIOFORMAT> {
  let mut format = UNCOMPRESSEDAUDIOFORMAT::default();
  // SAFETY: valid interface, out struct owned by us.
  unsafe { media.GetUncompressedAudioFormat(&mut format) }.ok()?;
  Some(format)
}

fn is_supported_format(format: &UNCOMPRESSEDAUDIOFORMAT) -> bool {
  format.guidFormatType == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    && format.dwBytesPerSampleContainer == 4
    && (1..=MAX_CHANNELS).contains(&format.dwSamplesPerFrame)
}

/// Copies input to output when they differ, then runs `process` in place on the output
/// samples of a valid buffer. Silent/invalid buffers keep their flags and are not processed.
///
/// # Safety
/// `pBuffer` of both connections must point to at least `u32ValidFrameCount * channels`
/// f32 samples (audio engine contract), or be 0.
pub(super) unsafe fn process_connection(
  input: &APO_CONNECTION_PROPERTY,
  output: &mut APO_CONNECTION_PROPERTY,
  channels: usize,
  process: impl FnOnce(&mut [f32]),
) {
  // SAFETY: the contract above; both buffers are non-zero and hold `samples` floats. Input and output may be the same
  // buffer: then only `out` exists, else they do not overlap (in-place APO or separate buffers).
  unsafe {
    let samples = input.u32ValidFrameCount as usize * channels;
    output.u32ValidFrameCount = input.u32ValidFrameCount;
    output.u32BufferFlags = input.u32BufferFlags;
    if input.pBuffer == 0 || output.pBuffer == 0 || samples == 0 {
      return;
    }

    let same_buffer = input.pBuffer == output.pBuffer;
    let out = std::slice::from_raw_parts_mut(output.pBuffer as *mut f32, samples);
    if input.u32BufferFlags == BUFFER_SILENT {
      if !same_buffer {
        out.fill(0.0);
      }
      return;
    }
    if input.u32BufferFlags != BUFFER_VALID {
      return;
    }
    if !same_buffer {
      out.copy_from_slice(std::slice::from_raw_parts(input.pBuffer as *const f32, samples));
    }
    process(out);
  }
}
