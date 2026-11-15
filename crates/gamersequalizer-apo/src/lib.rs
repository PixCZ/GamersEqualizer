//! System effect APOs that audiodg.exe loads on physical endpoints, three COM classes in one DLL:
//! - Mic chain (capture): AEC -> denoise -> EQ -> boost, from `gamersequalizer-mic-chain`, so
//!   every app receives processed audio without a virtual sound card.
//! - Output EQ (render): headphone correction + game profile as one biquad cascade per channel,
//!   at any sample rate. The bands come from the daemon through the same Global mapping.
//! - Stream EQ (render SFX, one instance per app stream, before the mix): publishes the
//!   stream's level envelope in the stream table; the daemon pairs it with an app and writes
//!   back its channel. Chat / media streams then run their channel's EQ (channel profile +
//!   inverted game profile), crossfaded in and out; everything else passes through.
//!
//! Runs inside audiodg: a crash kills audio for the whole system. Build with the `apo`
//! profile (panic = "unwind"); every call into the DSP chain is wrapped in `catch_unwind`, and
//! after a panic the APO passes audio through unchanged. The real-time path never blocks
//! (`try_lock` only).
//!
//! Settings, the speaker reference and AEC telemetry go through the Global mapping
//! `APO_SHM_NAME`, which this DLL creates (audiodg runs in session 0 as LOCAL SERVICE, which
//! may create Global objects; the user's daemon may not, so it opens it).
//!
//! The audio engine creates APOs *aggregated* (pUnkOuter != NULL) and refuses an APO that
//! returns CLASS_E_NOAGGREGATION, which disables the whole capture endpoint. windows-rs
//! `#[implement]` cannot be aggregated, so the APO object below is a hand-written COM object:
//! one vtable pointer per interface, all delegating IUnknown to the controlling unknown, plus
//! a non-delegating inner IUnknown that owns the reference count.
//!
//! Limits:
//! - Only 32-bit float formats are accepted (what the audio engine uses for system effects).
//! - Mic: the DSP chain runs at 48 kHz (the reference ring's rate); other rates (8-384 kHz)
//!   are resampled around it by `rate_bridge` (~5 ms extra latency), anything else passes
//!   through. Mono processing of the first channel, written to every channel.
//! - Output: a new band set is read (allocating) in the real-time thread, only when its
//!   version changes; switching EQ on/off is not crossfaded (may click once).
//! - Stream: same allocation on a new band set. A channel change is crossfaded (20 ms); an edit
//!   of the same channel's EQ is applied directly (filter state kept, like the output EQ).
#![deny(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::todo,
  clippy::unimplemented,
  clippy::indexing_slicing
)]

mod event_log;
mod rate_bridge;

use event_log::Level;
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{addr_of, addr_of_mut, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::limiter::{PeakLimiter, soft_limit};
use gamersequalizer_common::shm::{APO_SHM_NAME, OwnerWatch, REF_SAMPLE_RATE, ShmHeader, ShmLayout};
use gamersequalizer_common::sidetone::{NoiseMode, SidetoneChain, SidetoneReader, device_hash};
use gamersequalizer_common::streams::{StreamBus, StreamInfo};
use gamersequalizer_mic_chain::sidetone_denoise::{self, SidetoneDenoiser};
use gamersequalizer_mic_chain::{MicChain, SharedMemoryClient};
use rate_bridge::RateBridge;
use windows::Win32::Foundation::{
  CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER, PROPERTYKEY,
  S_FALSE, S_OK,
};
use windows::Win32::Media::Audio::Apo::{
  APO_CONNECTION_DESCRIPTOR, APO_CONNECTION_PROPERTY, APO_CONNECTION_PROPERTY_V2, APO_FLAG, APO_FLAG_DEFAULT,
  APO_FLAG_INPLACE, APO_REG_PROPERTIES, APOERR_ALREADY_UNLOCKED, APOERR_APO_LOCKED, APOERR_FORMAT_NOT_SUPPORTED,
  APOERR_NUM_CONNECTIONS_INVALID, APOInitSystemEffects, APOInitSystemEffects2, BUFFER_SILENT, BUFFER_VALID,
  IAudioMediaType, IAudioProcessingObject, IAudioProcessingObject_Vtbl, IAudioProcessingObjectConfiguration,
  IAudioProcessingObjectConfiguration_Vtbl, IAudioProcessingObjectRT, IAudioProcessingObjectRT_Vtbl,
  IAudioSystemEffects, IAudioSystemEffects_Vtbl, UNCOMPRESSEDAUDIOFORMAT,
};
use windows::Win32::Media::Audio::IMMDeviceCollection;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::{BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Ref, Result, implement};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// Mic chain. Must match `$Clsid` (Capture) in `scripts/apo-install.ps1`.
pub const CLSID_MIC_APO: GUID = GUID::from_u128(0x17e8d42e_dc01_4bd0_90ff_ad7c83a5ddea);
/// Output EQ. Must match `$Clsid` (Render) in `scripts/apo-install.ps1`.
pub const CLSID_OUTPUT_EQ_APO: GUID = GUID::from_u128(0x6b0f3c1e_8f2a_4d7b_9a51_3e2c7d4b9f10);
/// Stream probe. Must match `$Clsid` (Render, -Slot SFX) in `scripts/apo-install.ps1`.
pub const CLSID_STREAM_APO: GUID = GUID::from_u128(0x3c9a5e21_7d4f_4b8e_a1c6_5f2e8d9b0a47);

/// Sidetone telemetry (peak, gate, mode) is published every this many blocks.
const SIDETONE_TELEMETRY_BLOCKS: u32 = 10;

/// Global mapping DACL: full access for SYSTEM, LOCAL SERVICE (audiodg) and Administrators;
/// interactively logged-on users (the daemon) only map read/write (`0x6` =
/// SECTION_MAP_WRITE | SECTION_MAP_READ), so they cannot rewrite the DACL and lock audiodg out.
/// Known limit: every interactive user on the PC still shares one mapping (not supported for
/// several users at once, see README "Known limits"); per-user mappings would be the upgrade.
const APO_SHM_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;0x6;;;IU)";

/// `PKEY_AudioEndpoint_GUID` (`{1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E}, 4`): carried on
/// `APOInitSystemEffects.pAPOEndpointProperties` in Windows Audio Engine (`audiodg.exe`).
const PKEY_AUDIO_ENDPOINT_GUID: PROPERTYKEY =
  PROPERTYKEY { fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e), pid: 4 };
static APO_INSTANCE_IDS: AtomicU32 = AtomicU32::new(1);

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const MAX_CHANNELS: u32 = 32;
/// `APO_CONNECTION_PROPERTY_V2_SIGNATURE` ('ACP2'): the connection carries a QPC timestamp.
const CONNECTION_V2_SIGNATURE: u32 = 0x4143_5032;
const HNS_PER_SEC: i128 = 10_000_000;
/// A V2 timestamp further than this from "now" is ignored (unknown units / garbage).
const MAX_TIMESTAMP_SKEW_HNS: u64 = 10_000_000;
/// The mic clock re-anchors when a block's time is this far off (stream gap / glitch).
const CLOCK_RESYNC_HNS: i64 = 500_000;
/// Fraction of the timestamp error corrected per block (~2.5 s time constant at 10 ms
/// blocks): filters scheduling jitter of the engine's call times.
const CLOCK_CORRECTION_DIVISOR: i64 = 256;

const COPYRIGHT: &str = "GamersEqualizer";
/// Stream EQ crossfade when a stream changes channel (e.g. once it is paired).
const STREAM_FADE_SECONDS: f32 = 0.02;
/// Mic calls per published jitter peak (~1 s at 10 ms blocks).
const CALL_TIMING_WINDOW_CALLS: u32 = 100;

const OFF_APO: usize = offset_of!(ApoObject, apo);
const OFF_RT: usize = offset_of!(ApoObject, rt);
const OFF_CFG: usize = offset_of!(ApoObject, cfg);
const OFF_SFX: usize = offset_of!(ApoObject, sfx);
const OFF_INNER: usize = offset_of!(ApoObject, inner);

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// Which COM class an object was created as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApoKind {
  Mic,
  Output,
  Stream,
}

/// Processing state shared by all interfaces; atomics because the engine calls
/// configuration and real-time methods from different threads.
struct ApoState {
  kind: ApoKind,
  instance_id: u32,
  /// Endpoint GUID (`{...}`, lowercased) from `Initialize`, when Windows supplies it.
  endpoint_tag: OnceLock<String>,
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
  /// Boxed: the mic chain is kilobytes, the output EQ a few pointers.
  Mic(Box<MicProcessor>),
  Output(OutputProcessor),
  Stream(StreamProcessor),
}

/// Stream EQ: records the level envelope into its stream-table slot and runs its channel's
/// EQ once the daemon has paired it.
struct StreamProcessor {
  shm: SharedMemoryClient,
  /// (slot index, owner value); None = table full, run unregistered (pass-through).
  slot: Option<(usize, u64)>,
  sample_rate: f32,
  /// Shared coefficients with per-channel filter state, for the bus/version in `built`.
  cascade: BiquadCascade,
  preamp_gain: f32,
  /// (bus, stream EQ version) the cascade holds; None = nothing built (pass-through).
  built: Option<(StreamBus, u32)>,
  /// Share of the processed signal in the output, 0..1, moved by `fade_step` per frame.
  wet: f32,
  fade_step: f32,
  /// Processed copy while crossfading. Grows (allocates) only if the engine sends a
  /// larger buffer than before.
  scratch: Vec<f32>,
  /// No live owner (see `OwnerWatch`): treated like the master switch, the EQ fades out.
  owner: OwnerWatch,
  failed: bool,
}

/// Output EQ: shared coefficients with per-channel state, for the stream's sample rate.
struct OutputProcessor {
  shm: Option<SharedMemoryClient>,
  sample_rate: f32,
  cascade: BiquadCascade,
  preamp_gain: f32,
  /// Catches peaks above 0 dBFS from loudness matching / user gain.
  limiter: PeakLimiter,
  /// `out_eq_version` the cascade was built from.
  version: Option<u32>,
  /// Anything to do (bands or a preamp); false = pass through.
  active: bool,
  owner: OwnerWatch,
  failed: bool,
  /// Share of the processed signal, 0..1. Losing the owner (service hung, resume from sleep)
  /// or a bypass toggle fades over `STREAM_FADE_SECONDS` instead of jumping (a -6 dB preamp
  /// switching to 0 dB at once, the limiter dropping out mid-peak).
  wet: f32,
  fade_step: f32,
  /// Processed copy of the block while fading.
  scratch: Vec<f32>,
  /// Sidetone: reader of the mic APO's ring, its mono block, and this endpoint's hash
  /// (0 = unknown: never plays).
  sidetone: SidetoneReader,
  sidetone_buf: Vec<f32>,
  sidetone_endpoint: u32,
  /// Sidetone was mixed into the previous block.
  sidetone_playing: bool,
}

/// The mic DSP chain plus its scratch buffers.
/// Mic chains alive in this process: one per microphone in EFX, one per processing mode in
/// use in MFX. The "started" note shows it, so two chains on one mic are visible.
static MIC_CHAINS: AtomicU32 = AtomicU32::new(0);
/// Id of the mic chain whose telemetry the app shows (0 = none): the first one to process.
/// Several would overwrite each other's values in the shared memory.
static MIC_PUBLISHER: AtomicU32 = AtomicU32::new(0);
/// Bumped by the publisher on every block: the others see it is still being called.
static MIC_PUBLISHER_BEAT: AtomicU32 = AtomicU32::new(0);
/// Source of mic chain ids.
static MIC_CHAIN_IDS: AtomicU32 = AtomicU32::new(0);
/// Blocks (~0.5 s) without the publisher's beat after which another chain takes over: the
/// publisher failed, or its app paused the stream without closing it.
const PUBLISHER_QUIET_BLOCKS: u32 = 50;

/// One live mic chain: counted in `MIC_CHAINS`, and maybe the publisher, for as long as it exists.
struct MicChainCount {
  id: u32,
  /// The publisher's beat this chain saw last, and its own blocks since it last changed.
  seen_beat: u32,
  quiet_blocks: u32,
}

struct MicProcessor {
  chain: Box<MicChain>,
  sidetone: Box<SidetoneTap>,
  mono_in: Vec<f32>,
  mono_out: Vec<f32>,
  /// Resampling to/from 48 kHz when the mic runs at another rate; None at 48 kHz.
  bridge: Option<RateBridge>,
  /// Set after a panic in the chain: pass audio through from then on.
  failed: bool,
  clock: MicClock,
  owner: OwnerWatch,
  /// Pass through unless a daemon owns the mapping. Only tests turn it off (no daemon there).
  follow_owner: bool,
  /// The chain ran on the previous block. After a pass-through stretch the bridge FIFOs hold
  /// audio from before it.
  was_live: bool,
  timing: CallTiming,
  /// Blocks the engine handed over / the chain processed, for the note when the stream stops.
  blocks: u64,
  processed: u64,
  count: MicChainCount,
  endpoint_tag: Option<String>,
  instance_id: u32,
}

/// Sidetone tap of the mic APO: raw input -> EQ -> noise suppression -> level -> ring.
/// Independent of the mic chain: it runs when the chain failed and only reads the buffer.
struct SidetoneTap {
  chain: Box<SidetoneChain>,
  /// `sidetone_eq_version` the chain's EQ was built from.
  eq_version: Option<u32>,
  /// Frame-based suppressors (48 kHz only); None at another rate: the expander stands in.
  denoiser: Option<Box<SidetoneDenoiser>>,
  rate: u32,
  /// `device_hash` of the endpoint this APO sits on; 0 = unknown.
  endpoint_hash: u32,
  instance: u32,
  /// Owner value while this tap writes the ring.
  owner: Option<u64>,
  mono: Vec<f32>,
  peak: f32,
  blocks: u32,
  failed: bool,
}

/// How regularly the audio engine calls the mic APO (debug log): an app gets its data late
/// when a call comes late, e.g. because the previous one blocked.
#[derive(Default)]
struct CallTiming {
  last: Option<Instant>,
  calls: u32,
  jitter_peak_us: u64,
}

/// Capture time of each block from a sample counter, slowly steered towards the raw
/// timestamps. Raw APO call times jitter by milliseconds, and the delay estimator needs
/// windows that agree within 2 ms, and the AEC needs a steady alignment.
struct MicClock {
  /// Device frames per second (what `next` counts in).
  rate: u32,
  /// (time of frame 0 in hns, frames since then)
  anchor: Option<(i64, u64)>,
}

// SAFETY: the chain holds `Rc`s internally (DeepFilterNet's tract graph) that never leave
// it; the whole Processor moves between the configuration and real-time threads as one
// unit behind a Mutex, so no Rc is ever touched from two threads at once.
unsafe impl Send for MicProcessor {}

/// Hand-written aggregatable COM object. Interface pointers handed out are addresses of the
/// vtable-pointer fields; each method recovers the object by subtracting the field offset.
#[repr(C)]
struct ApoObject {
  apo: *const IAudioProcessingObject_Vtbl,
  rt: *const IAudioProcessingObjectRT_Vtbl,
  cfg: *const IAudioProcessingObjectConfiguration_Vtbl,
  sfx: *const IAudioSystemEffects_Vtbl,
  /// Non-delegating IUnknown: owns `refs`, answers QueryInterface.
  inner: *const IUnknown_Vtbl,
  refs: AtomicU32,
  /// Controlling unknown: the aggregating outer object (not AddRef'd, per COM aggregation
  /// rules), or our own `inner` when not aggregated.
  outer: *mut c_void,
  state: ApoState,
}

#[implement(IClassFactory)]
struct ApoFactory {
  kind: ApoKind,
}

// ---------------------------------------------------------------------------------------
// Vtables
// ---------------------------------------------------------------------------------------

static APO_VTBL: IAudioProcessingObject_Vtbl = IAudioProcessingObject_Vtbl {
  base__: delegating_unknown::<OFF_APO>(),
  Reset: apo_reset,
  GetLatency: apo_get_latency,
  GetRegistrationProperties: apo_get_registration_properties,
  Initialize: apo_initialize,
  IsInputFormatSupported: apo_is_input_format_supported,
  IsOutputFormatSupported: apo_is_output_format_supported,
  GetInputChannelCount: apo_get_input_channel_count,
};

static RT_VTBL: IAudioProcessingObjectRT_Vtbl = IAudioProcessingObjectRT_Vtbl {
  base__: delegating_unknown::<OFF_RT>(),
  APOProcess: rt_apo_process,
  CalcInputFrames: rt_calc_frames,
  CalcOutputFrames: rt_calc_frames,
};

static CFG_VTBL: IAudioProcessingObjectConfiguration_Vtbl = IAudioProcessingObjectConfiguration_Vtbl {
  base__: delegating_unknown::<OFF_CFG>(),
  LockForProcess: cfg_lock_for_process,
  UnlockForProcess: cfg_unlock_for_process,
};

static SFX_VTBL: IAudioSystemEffects_Vtbl = IAudioSystemEffects_Vtbl { base__: delegating_unknown::<OFF_SFX>() };

static INNER_VTBL: IUnknown_Vtbl =
  IUnknown_Vtbl { QueryInterface: inner_query_interface, AddRef: inner_add_ref, Release: inner_release };

// ---------------------------------------------------------------------------------------
// DLL exports
// ---------------------------------------------------------------------------------------

/// # Safety
/// Called by COM with pointers it owns; every pointer is null-checked before use.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
  rclsid: *const GUID,
  riid: *const GUID,
  ppv: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: COM passes its own pointers or null; each is null-checked before it is read or written, and `query`
  // writes the interface to the checked `ppv`.
  unsafe {
    if ppv.is_null() {
      return E_POINTER;
    }
    *ppv = null_mut();
    if rclsid.is_null() || riid.is_null() {
      return E_INVALIDARG;
    }
    let Some(kind) = ApoKind::from_clsid(&*rclsid) else {
      return CLASS_E_CLASSNOTAVAILABLE;
    };
    // Each step Windows takes towards a running effect is noted once, so the last note shows
    // where it stopped (class -> instance -> initialized -> formats -> stream).
    note(Level::Info, format!("{}: class requested", kind.friendly_name()));
    let factory: IClassFactory = ApoFactory { kind }.into();
    factory.query(riid, ppv)
  }
}

/// Shortcut: never unload. audiodg keeps APOs alive anyway; tracking live objects for a
/// correct answer is not worth it until the DLL has to be upgraded without an audio restart.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
  S_FALSE
}

// ---------------------------------------------------------------------------------------
// Class factory
// ---------------------------------------------------------------------------------------

impl IClassFactory_Impl for ApoFactory_Impl {
  fn CreateInstance(&self, outer: Ref<'_, IUnknown>, riid: *const GUID, object: *mut *mut c_void) -> Result<()> {
    let outer = outer.as_ref();
    if object.is_null() || riid.is_null() {
      return Err(E_POINTER.into());
    }
    // SAFETY: both pointers checked non-null above; COM owns them.
    unsafe {
      *object = null_mut();
      let name = self.kind.friendly_name();
      // An aggregated object may only be asked for its inner IUnknown.
      if outer.is_some() && *riid != IUnknown::IID {
        note(Level::Warning, format!("{name}: refused an aggregated instance asked for {:?}", *riid));
        return Err(CLASS_E_NOAGGREGATION.into());
      }
      let inner = ApoObject::create(outer.map(|o| o.as_raw()), self.kind);
      let hr = inner_query_interface(inner, riid, object);
      inner_release(inner); // drop the creation reference; frees the object if QI failed
      let how = if outer.is_some() { "aggregated" } else { "not aggregated" };
      let level = if hr.is_ok() { Level::Info } else { Level::Warning };
      note(level, format!("{name}: instance created ({how}): {hr:?}"));
      hr.ok()
    }
  }

  fn LockServer(&self, _lock: BOOL) -> Result<()> {
    Ok(())
  }
}

// ---------------------------------------------------------------------------------------
// COM object plumbing
// ---------------------------------------------------------------------------------------

impl ApoObject {
  /// Allocates the object with one reference and returns its inner IUnknown pointer.
  fn create(outer: Option<*mut c_void>, kind: ApoKind) -> *mut c_void {
    let obj = Box::into_raw(Box::new(ApoObject {
      apo: &APO_VTBL,
      rt: &RT_VTBL,
      cfg: &CFG_VTBL,
      sfx: &SFX_VTBL,
      inner: &INNER_VTBL,
      refs: AtomicU32::new(1),
      outer: null_mut(),
      state: ApoState::new(kind),
    }));
    // SAFETY: obj was just allocated and is uniquely owned here.
    unsafe {
      let inner = addr_of_mut!((*obj).inner) as *mut c_void;
      (*obj).outer = outer.unwrap_or(inner);
      inner
    }
  }
}

/// # Safety
/// `this` must be an interface pointer of an `ApoObject` taken at field offset `OFF`.
unsafe fn object<'a, const OFF: usize>(this: *mut c_void) -> &'a ApoObject {
  // SAFETY: by the contract above, `this - OFF` is the start of a live `ApoObject`.
  unsafe { &*((this as *mut u8).sub(OFF) as *const ApoObject) }
}

/// # Safety
/// As `object`.
unsafe fn state<'a, const OFF: usize>(this: *mut c_void) -> &'a ApoState {
  // SAFETY: the contract of `object`, passed on.
  unsafe { &object::<OFF>(this).state }
}

/// # Safety
/// `unknown` is a live COM interface pointer.
unsafe fn unknown_vtbl(unknown: *mut c_void) -> &'static IUnknown_Vtbl {
  // SAFETY: the contract above: a COM interface pointer points to its vtable pointer, and callers use
  // the vtable only while they hold the interface.
  unsafe { &**(unknown as *const *const IUnknown_Vtbl) }
}

const fn delegating_unknown<const OFF: usize>() -> IUnknown_Vtbl {
  IUnknown_Vtbl {
    QueryInterface: delegating_qi::<OFF>,
    AddRef: delegating_add_ref::<OFF>,
    Release: delegating_release::<OFF>,
  }
}

unsafe extern "system" fn delegating_qi<const OFF: usize>(
  this: *mut c_void,
  iid: *const GUID,
  ppv: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF` of a live `ApoObject` and holds a
  // reference for the call (`object`); `outer` is the controlling IUnknown, alive while this object is (aggregation),
  // or our own inner one.
  unsafe {
    let outer = object::<OFF>(this).outer;
    (unknown_vtbl(outer).QueryInterface)(outer, iid, ppv)
  }
}

unsafe extern "system" fn delegating_add_ref<const OFF: usize>(this: *mut c_void) -> u32 {
  // SAFETY: as `delegating_qi`.
  unsafe {
    let outer = object::<OFF>(this).outer;
    (unknown_vtbl(outer).AddRef)(outer)
  }
}

unsafe extern "system" fn delegating_release<const OFF: usize>(this: *mut c_void) -> u32 {
  // SAFETY: as `delegating_qi`.
  unsafe {
    let outer = object::<OFF>(this).outer;
    (unknown_vtbl(outer).Release)(outer)
  }
}

unsafe extern "system" fn inner_query_interface(this: *mut c_void, iid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_INNER` of a live `ApoObject` and holds a
  // reference for the call (`object`); `ppv` and `iid` are null-checked, and `found` is one of this object's
  // interface fields.
  unsafe {
    if ppv.is_null() {
      return E_POINTER;
    }
    *ppv = null_mut();
    if iid.is_null() {
      return E_POINTER;
    }
    let obj = (this as *mut u8).sub(OFF_INNER) as *mut ApoObject;
    let iid = *iid;
    let found: *mut c_void = if iid == IUnknown::IID {
      addr_of_mut!((*obj).inner) as *mut c_void
    } else if iid == IAudioProcessingObject::IID {
      addr_of_mut!((*obj).apo) as *mut c_void
    } else if iid == IAudioProcessingObjectRT::IID {
      addr_of_mut!((*obj).rt) as *mut c_void
    } else if iid == IAudioProcessingObjectConfiguration::IID {
      addr_of_mut!((*obj).cfg) as *mut c_void
    } else if iid == IAudioSystemEffects::IID {
      addr_of_mut!((*obj).sfx) as *mut c_void
    } else {
      return E_NOINTERFACE;
    };
    // AddRef through the returned interface: outer-delegating except for the inner IUnknown.
    (unknown_vtbl(found).AddRef)(found);
    *ppv = found;
    S_OK
  }
}

unsafe extern "system" fn inner_add_ref(this: *mut c_void) -> u32 {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_INNER` of a live `ApoObject` and holds a
  // reference for the call (`object`).
  unsafe { object::<OFF_INNER>(this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn inner_release(this: *mut c_void) -> u32 {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_INNER` of a live `ApoObject` and holds a
  // reference for the call (`object`). The last reference frees the `Box` from `ApoObject::create`, once: no caller
  // may use the pointer after its final Release.
  unsafe {
    let left = object::<OFF_INNER>(this).refs.fetch_sub(1, Ordering::AcqRel).wrapping_sub(1);
    if left == 0 {
      let obj = Box::from_raw((this as *mut u8).sub(OFF_INNER) as *mut ApoObject);
      // Windows may release a still-locked APO (device removed): this drops the processor, and
      // a panic in a DSP destructor must not unwind out of `extern "system"` (kills audiodg).
      let _ = catch_unwind(AssertUnwindSafe(move || drop(obj)));
    }
    left
  }
}

// ---------------------------------------------------------------------------------------
// IAudioProcessingObject
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn apo_reset(_this: *mut c_void) -> HRESULT {
  S_OK
}

unsafe extern "system" fn apo_get_latency(_this: *mut c_void, latency: *mut i64) -> HRESULT {
  // SAFETY: `latency` is null-checked; the engine owns it for the call.
  unsafe {
    if latency.is_null() {
      return E_POINTER;
    }
    *latency = 0;
    S_OK
  }
}

unsafe extern "system" fn apo_get_registration_properties(
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

unsafe extern "system" fn apo_initialize(this: *mut c_void, size: u32, data: *const u8) -> HRESULT {
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
}

unsafe extern "system" fn apo_is_input_format_supported(
  this: *mut c_void,
  opposite: *mut c_void,
  requested: *mut c_void,
  supported: *mut *mut c_void,
) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_APO` of a live `ApoObject` and holds a
  // reference for the call (`object`); the media types are the engine's (contract of `is_format_supported`).
  unsafe { is_format_supported(this, "input", opposite, requested, supported) }
}

unsafe extern "system" fn apo_is_output_format_supported(
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

unsafe extern "system" fn apo_get_input_channel_count(this: *mut c_void, count: *mut u32) -> HRESULT {
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

// ---------------------------------------------------------------------------------------
// IAudioProcessingObjectRT
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn rt_apo_process(
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
    // Only the mic chain lines its audio up with the reference by time.
    let qpc_hns = (st.kind == ApoKind::Mic).then(|| capture_time_hns(input, st.rate.load(Ordering::Relaxed))).flatten();
    process_connection(&*input, &mut *output, channels, |buf| {
      // Never block the audio thread: if configuration holds the lock, pass this block through.
      if let Ok(mut guard) = st.processor.try_lock()
        && let Some(processor) = guard.as_mut()
      {
        processor.process(buf, channels, qpc_hns);
      }
    });
  }
}

unsafe extern "system" fn rt_calc_frames(_this: *mut c_void, frames: u32) -> u32 {
  frames
}

// ---------------------------------------------------------------------------------------
// IAudioProcessingObjectConfiguration
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn cfg_lock_for_process(
  this: *mut c_void,
  num_inputs: u32,
  inputs: *const *const APO_CONNECTION_DESCRIPTOR,
  num_outputs: u32,
  outputs: *const *const APO_CONNECTION_DESCRIPTOR,
) -> HRESULT {
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
      note_for(st, Level::Warning, format!("stream format not supported ({what}); Windows runs the stream without it"));
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
      ApoKind::Output => OutputProcessor::create(shared_memory(st), rate, format.dwSamplesPerFrame).map(|mut p| {
        p.sidetone_endpoint = endpoint;
        Processor::Output(p)
      }),
      ApoKind::Stream => shared_memory(st)
        .and_then(|shm| StreamProcessor::create(shm, rate, format.dwSamplesPerFrame, mode))
        .map(Processor::Stream),
    };
    let level = if processor.is_some() { Level::Info } else { Level::Warning };
    let outcome = match &processor {
      Some(Processor::Mic(_)) => format!("started, mic chains running here: {}", MIC_CHAINS.load(Ordering::Relaxed)),
      Some(_) => "started".to_string(),
      None => "no chain, passes audio through".to_string(),
    };
    let msg = format!("stream {}, mode {:?}: {outcome}", describe_format(&format), GUID::from_u128(mode));
    note_for(st, level, msg);
    if let Ok(mut slot) = st.processor.lock() {
      // Replacing drops a previous processor: keep a destructor panic inside this DLL.
      let old = std::mem::replace(&mut *slot, processor);
      let _ = catch_unwind(AssertUnwindSafe(move || drop(old)));
    }
    st.locked.store(true, Ordering::Release);
    S_OK
  }
}

unsafe extern "system" fn cfg_unlock_for_process(this: *mut c_void) -> HRESULT {
  // SAFETY: COM calls this slot only through our interface pointer at `OFF_CFG` of a live `ApoObject` and holds a
  // reference for the call (`object`).
  unsafe {
    let st = state::<OFF_CFG>(this);
    if !st.locked.swap(false, Ordering::AcqRel) {
      return APOERR_ALREADY_UNLOCKED;
    }
    if let Ok(mut slot) = st.processor.lock() {
      if let Some(Processor::Mic(mic)) = slot.as_ref() {
        mic.note_stop();
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

fn shared_memory_tagged(kind: ApoKind, endpoint_tag: Option<&str>, instance_id: u32) -> Option<SharedMemoryClient> {
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

/// An Application log note scoped per endpoint GUID (or instance id fallback when Windows
/// did not pass one): same format on multiple devices never suppresses another device's notes.
fn note_for(st: &ApoState, level: Level, msg: String) {
  note_tagged(st.kind, st.endpoint_tag.get().map(String::as_str), st.instance_id, level, msg);
}

fn note_tagged(kind: ApoKind, endpoint_tag: Option<&str>, instance_id: u32, level: Level, msg: String) {
  let prefix = match endpoint_tag {
    Some(tag) => format!("{} [{tag}]", kind.friendly_name()),
    None => kind.friendly_name().to_string(),
  };
  let text = format!("{prefix}: {msg}");
  let key = if endpoint_tag.is_some() { text.clone() } else { format!("#{instance_id}:{text}") };
  event_log::note(level, &key, &text);
}

/// An Application log note keyed by its own text: each distinct text once per process.
fn note(level: Level, text: String) {
  event_log::note(level, &text, &text);
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
  fn from_clsid(clsid: &GUID) -> Option<Self> {
    match *clsid {
      CLSID_MIC_APO => Some(ApoKind::Mic),
      CLSID_OUTPUT_EQ_APO => Some(ApoKind::Output),
      CLSID_STREAM_APO => Some(ApoKind::Stream),
      _ => None,
    }
  }

  fn clsid(self) -> GUID {
    match self {
      ApoKind::Mic => CLSID_MIC_APO,
      ApoKind::Output => CLSID_OUTPUT_EQ_APO,
      ApoKind::Stream => CLSID_STREAM_APO,
    }
  }

  fn friendly_name(self) -> &'static str {
    match self {
      ApoKind::Mic => "GamersEqualizer Mic Chain",
      ApoKind::Output => "GamersEqualizer Output EQ",
      ApoKind::Stream => "GamersEqualizer Stream EQ",
    }
  }
}

impl ApoState {
  fn new(kind: ApoKind) -> Self {
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
  fn process(&mut self, buf: &mut [f32], channels: usize, qpc_hns: Option<u64>) {
    match self {
      Processor::Mic(p) => p.process(buf, channels, qpc_hns),
      Processor::Output(p) => p.process(buf, channels),
      Processor::Stream(p) => p.process(buf, channels),
    }
  }
}

impl StreamProcessor {
  /// Claims a stream-table slot. None only if building panicked (pass-through).
  fn create(shm: SharedMemoryClient, sample_rate: f32, channels: u32, mode: u128) -> Option<Self> {
    catch_unwind(AssertUnwindSafe(move || {
      // SAFETY: no preconditions.
      let pid = unsafe { GetCurrentProcessId() };
      let info = StreamInfo { pid, now_hns: now_hns().unwrap_or(0), sample_rate: sample_rate as u32, channels, mode };
      let slot = shm.layout().streams.claim(&info);
      StreamProcessor {
        shm,
        slot,
        sample_rate,
        cascade: BiquadCascade::default(),
        preamp_gain: 1.0,
        built: None,
        wet: 0.0,
        fade_step: 1.0 / (STREAM_FADE_SECONDS * sample_rate.max(1.0)),
        scratch: Vec::with_capacity(4096),
        owner: OwnerWatch::default(),
        failed: false,
      }
    }))
    .ok()
  }

  /// Records the input level (the daemon pairs on it), then runs the channel EQ in place.
  fn process(&mut self, buf: &mut [f32], channels: usize) {
    if channels == 0 {
      return;
    }
    let Some((index, _)) = self.slot else {
      return;
    };
    let Some(slot) = self.shm.layout().streams.slots.get(index) else {
      return;
    };
    if let Some(now) = now_hns() {
      let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      slot.record(now, peak, (buf.len() / channels) as u32);
    }
    if self.failed {
      return;
    }
    let bus = StreamBus::from_u32(slot.bus.load(Ordering::Relaxed));
    if catch_unwind(AssertUnwindSafe(|| self.process_eq(buf, channels, bus))).is_err() {
      self.failed = true;
    }
  }

  fn process_eq(&mut self, buf: &mut [f32], channels: usize, bus: StreamBus) {
    let h: &ShmHeader = &self.shm.layout().header;
    let owned = self.owner.live(h, Instant::now());
    let target = match bus.eq_index().and_then(|i| h.stream_eq.get(i)) {
      Some(eq) if owned && !h.master_bypass.load(Ordering::Relaxed) => Some((bus, eq.version.load(Ordering::Acquire))),
      _ => None,
    };
    match (self.built, target) {
      (built, target) if built == target => {}
      // Same channel, edited EQ: new coefficients, filter state kept.
      (Some((old_bus, _)), Some((new_bus, _))) if old_bus == new_bus => self.build(target, channels, false),
      // Channel change: fade the old EQ out first, then start the new one from silence.
      _ if self.wet <= 0.0 => self.build(target, channels, true),
      _ => {}
    }
    let goal = if self.built.is_some() && self.built == target { 1.0 } else { 0.0 };
    if self.built.is_none() || (self.wet <= 0.0 && goal <= 0.0) {
      self.wet = 0.0;
      return;
    }
    if self.wet >= 1.0 && goal >= 1.0 {
      self.run_eq_in_place(buf, channels);
      return;
    }
    // Crossfade: dry + wet * (processed - dry), wet ramping per frame.
    if self.scratch.len() < buf.len() {
      self.scratch.resize(buf.len(), 0.0);
    }
    let mut scratch = std::mem::take(&mut self.scratch);
    if let Some(wet_buf) = scratch.get_mut(..buf.len()) {
      wet_buf.copy_from_slice(buf);
      self.run_eq_in_place(wet_buf, channels);
      for (dry_frame, wet_frame) in buf.chunks_exact_mut(channels).zip(wet_buf.chunks_exact(channels)) {
        self.wet =
          if goal > self.wet { (self.wet + self.fade_step).min(1.0) } else { (self.wet - self.fade_step).max(0.0) };
        for (d, w) in dry_frame.iter_mut().zip(wet_frame) {
          *d += self.wet * (w - *d);
        }
      }
    }
    self.scratch = scratch;
  }

  /// Load the bands of `target` (allocates; only when its version changed).
  fn build(&mut self, target: Option<(StreamBus, u32)>, channels: usize, fresh: bool) {
    self.built = target;
    let Some(eq) = target.and_then(|(bus, _)| bus.eq_index()).and_then(|i| self.shm.layout().header.stream_eq.get(i))
    else {
      self.cascade.clear();
      self.preamp_gain = 1.0;
      return;
    };
    let (bands, preamp_db) = eq.read();
    if fresh {
      self.cascade.reset_state();
    }
    self.cascade.set_channels(channels);
    self.cascade.set_bands(bands, self.sample_rate);
    self.preamp_gain = 10f32.powf(preamp_db / 20.0);
  }

  fn run_eq_in_place(&mut self, buf: &mut [f32], channels: usize) {
    if self.preamp_gain != 1.0 {
      buf.iter_mut().for_each(|s| *s *= self.preamp_gain);
    }
    self.cascade.process_interleaved(buf, channels);
  }
}

impl Drop for StreamProcessor {
  fn drop(&mut self) {
    if let Some((index, owner)) = self.slot {
      self.shm.layout().streams.release(index, owner);
    }
  }
}

impl OutputProcessor {
  /// None (pass-through) only if building panicked. Without the mapping it passes through.
  fn create(shm: Option<SharedMemoryClient>, sample_rate: f32, channels: u32) -> Option<Self> {
    catch_unwind(AssertUnwindSafe(move || OutputProcessor {
      shm,
      sample_rate,
      cascade: BiquadCascade::with_channels(channels as usize),
      preamp_gain: 1.0,
      limiter: PeakLimiter::new(sample_rate),
      version: None,
      active: false,
      owner: OwnerWatch::default(),
      failed: false,
      wet: 0.0,
      fade_step: 1.0 / (STREAM_FADE_SECONDS * sample_rate.max(1.0)),
      // ~85 ms of stereo at 48 kHz; grows only if the engine sends longer blocks.
      scratch: Vec::with_capacity(8192),
      sidetone: SidetoneReader::default(),
      sidetone_buf: Vec::with_capacity(4096),
      sidetone_endpoint: 0,
      sidetone_playing: false,
    }))
    .ok()
  }

  /// In place on interleaved `buf`: preamp, then each channel through its own cascade.
  fn process(&mut self, buf: &mut [f32], channels: usize) {
    if self.failed || channels == 0 {
      return;
    }
    let ran = catch_unwind(AssertUnwindSafe(|| self.process_inner(buf, channels)));
    if ran.is_err() {
      self.failed = true;
    }
  }

  fn process_inner(&mut self, buf: &mut [f32], channels: usize) {
    let Some(shm) = self.shm.as_ref() else {
      return;
    };
    let layout = shm.layout();
    let h: &ShmHeader = &layout.header;
    h.out_apo_blocks.fetch_add(1, Ordering::Relaxed);
    h.out_apo_sample_rate.store(self.sample_rate as u32, Ordering::Relaxed);
    h.out_apo_channels.store(channels as u32, Ordering::Relaxed);
    // Nobody's settings to apply (app not running for the user at the console): untouched.
    let enabled = self.owner.live(h, Instant::now()) && !h.master_bypass.load(Ordering::Relaxed);
    if enabled {
      let version = h.out_eq_version.load(Ordering::Acquire);
      if self.version != Some(version) {
        self.version = Some(version);
        let (bands, preamp_db) = h.read_out_eq();
        self.cascade.set_bands(bands, self.sample_rate);
        self.preamp_gain = 10f32.powf(preamp_db / 20.0);
        self.active = !self.cascade.is_empty() || preamp_db != 0.0;
      }
    }
    let goal = if enabled && self.active { 1.0 } else { 0.0 };
    // The mono sidetone block for this buffer is in `sidetone_buf` when this is true.
    let sidetone = read_sidetone(
      &mut self.sidetone,
      &mut self.sidetone_buf,
      layout,
      buf.len() / channels,
      self.sample_rate,
      self.sidetone_endpoint,
    );
    let sidetone_started = sidetone && !std::mem::replace(&mut self.sidetone_playing, sidetone);
    if self.wet <= 0.0 && goal <= 0.0 {
      if sidetone {
        if sidetone_started {
          // The limiter did not run while nothing was mixed in: its gain is from back then.
          self.limiter = PeakLimiter::new(self.sample_rate);
        }
        // No EQ, but the sum of the mix and the voice still must not clip.
        add_sidetone(buf, channels, &self.sidetone_buf);
        self.limiter.process(buf, channels);
      }
      return;
    }
    if self.wet <= 0.0 {
      // Fading in after a pass-through stretch: the filter memory is from back then.
      self.cascade.reset_state();
    }
    if self.wet >= 1.0 && goal >= 1.0 {
      // After the EQ (the voice gets no game profile), before the limiter (it guards the sum).
      self.run_eq(buf, channels);
      if sidetone {
        add_sidetone(buf, channels, &self.sidetone_buf);
      }
      self.limiter.process(buf, channels);
      return;
    }
    if self.scratch.len() < buf.len() {
      self.scratch.resize(buf.len(), 0.0);
    }
    let mut scratch = std::mem::take(&mut self.scratch);
    if let Some(wet_buf) = scratch.get_mut(..buf.len()) {
      wet_buf.copy_from_slice(buf);
      self.run_eq_in_place(wet_buf, channels);
      for (dry_frame, wet_frame) in buf.chunks_exact_mut(channels).zip(wet_buf.chunks_exact(channels)) {
        self.wet =
          if goal > self.wet { (self.wet + self.fade_step).min(1.0) } else { (self.wet - self.fade_step).max(0.0) };
        for (d, w) in dry_frame.iter_mut().zip(wet_frame) {
          *d += self.wet * (w - *d);
        }
      }
    }
    self.scratch = scratch;
    if sidetone {
      // shortcut: during the 20 ms EQ fade the sum only gets the static safety curve.
      add_sidetone(buf, channels, &self.sidetone_buf);
      buf.iter_mut().for_each(|s| *s = soft_limit(*s));
    }
  }

  /// Preamp and the cascades, without the limiter.
  fn run_eq(&mut self, buf: &mut [f32], channels: usize) {
    if self.preamp_gain != 1.0 {
      buf.iter_mut().for_each(|s| *s *= self.preamp_gain);
    }
    self.cascade.process_interleaved(buf, channels);
  }

  fn run_eq_in_place(&mut self, buf: &mut [f32], channels: usize) {
    self.run_eq(buf, channels);
    // Volume matching / user gain may let peaks pass 0 dBFS: turn them down instead of clipping.
    self.limiter.process(buf, channels);
  }
}

impl MicChainCount {
  fn new() -> Self {
    MIC_CHAINS.fetch_add(1, Ordering::Relaxed);
    // Never 0 ("no publisher"), also after the counter wraps.
    let id = MIC_CHAIN_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1);
    Self { id, seen_beat: 0, quiet_blocks: 0 }
  }

  /// One block of this chain: true when it is the one that publishes telemetry. It takes
  /// over when there is no publisher (the first chain; the previous one's stream closed) or
  /// the publisher went quiet. Atomics only: called on the real-time thread.
  fn publishes(&mut self) -> bool {
    let current = MIC_PUBLISHER.load(Ordering::Relaxed);
    if current == self.id {
      MIC_PUBLISHER_BEAT.fetch_add(1, Ordering::Relaxed);
      return true;
    }
    if current != 0 {
      let beat = MIC_PUBLISHER_BEAT.load(Ordering::Relaxed);
      if beat != self.seen_beat {
        self.seen_beat = beat;
        self.quiet_blocks = 0;
        return false;
      }
      self.quiet_blocks += 1;
      if self.quiet_blocks < PUBLISHER_QUIET_BLOCKS {
        return false;
      }
    }
    self.quiet_blocks = 0;
    MIC_PUBLISHER.compare_exchange(current, self.id, Ordering::AcqRel, Ordering::Relaxed).is_ok()
  }
}

impl Drop for MicChainCount {
  fn drop(&mut self) {
    let _ = MIC_PUBLISHER.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Relaxed);
    MIC_CHAINS.fetch_sub(1, Ordering::Relaxed);
  }
}

impl MicProcessor {
  /// Opens/creates the Global mapping and builds the chain for a `rate` Hz stream (resampled
  /// to 48 kHz unless it already is). None (pass-through) on an unsupported rate or failure.
  fn create(rate: u32, endpoint_tag: Option<String>, instance_id: u32) -> Option<Box<Self>> {
    let refuse = |why: String| {
      note_tagged(
        ApoKind::Mic,
        endpoint_tag.as_deref(),
        instance_id,
        Level::Warning,
        format!("{why}; passes audio through"),
      );
    };
    let bridge = match RateBridge::for_rate(rate) {
      Some(Ok(bridge)) => Some(bridge),
      Some(Err(e)) => {
        refuse(format!("resampler for {rate} Hz failed ({e})"));
        return None;
      }
      None if rate == REF_SAMPLE_RATE => None,
      None => {
        refuse(format!("{rate} Hz is not supported"));
        return None;
      }
    };
    let shm_tag = endpoint_tag.clone();
    let built = catch_unwind(AssertUnwindSafe(move || {
      let shm = shared_memory_tagged(ApoKind::Mic, shm_tag.as_deref(), instance_id);
      let mut chain = Box::new(MicChain::with_shm(shm));
      chain.preload_selected_model();
      #[cfg(feature = "debug-capture")]
      if let Ok(map) = gamersequalizer_mic_chain::debug_capture::DebugCaptureMap::create(APO_SHM_SDDL) {
        chain.attach_debug_capture(map, rate);
      }
      Box::new(MicProcessor {
        chain,
        sidetone: Box::new(SidetoneTap::new(rate, instance_id)),
        mono_in: vec![0.0; 4096],
        mono_out: vec![0.0; 4096],
        bridge,
        failed: false,
        clock: MicClock::new(rate),
        owner: OwnerWatch::default(),
        follow_owner: true,
        was_live: false,
        timing: CallTiming::default(),
        blocks: 0,
        processed: 0,
        count: MicChainCount::new(),
        endpoint_tag: shm_tag,
        instance_id,
      })
    }));
    if built.is_err() {
      refuse("building the chain panicked".to_string());
    }
    built.ok()
  }

  /// Application-log note when the stream stops: did the chain really run, and if not, why.
  /// Keyed per endpoint + reason, so each outcome is written once per endpoint per audiodg run.
  fn note_stop(&self) {
    let why = if self.failed {
      "the chain failed and passed audio through from then on"
    } else if self.blocks == 0 {
      "no audio blocks arrived"
    } else if self.processed > 0 {
      "processed"
    } else if self.chain.shm_header().is_none() {
      "no shared memory, passed audio through"
    } else {
      "GamersEqualizer did not drive it (not running for the user at the console), passed audio through"
    };
    let level = if self.processed > 0 { Level::Info } else { Level::Warning };
    let label = match &self.endpoint_tag {
      Some(tag) => format!("Mic chain [{tag}]"),
      None => "Mic chain".to_string(),
    };
    let text = format!("{label}: stream stopped after {} blocks, {} processed: {why}", self.blocks, self.processed);
    let id = self.endpoint_tag.clone().unwrap_or_else(|| format!("#{}", self.instance_id));
    event_log::note(level, &format!("mic stop:{id}:{why}"), &text);
    if self.sidetone.failed {
      let text = format!("{label}: the sidetone tap failed and stayed silent until this stream stopped");
      event_log::note(Level::Warning, &format!("sidetone failed:{id}"), &text);
    }
  }

  /// In place on interleaved `buf`: first channel through the chain, result to all channels.
  fn process(&mut self, buf: &mut [f32], channels: usize, qpc_hns: Option<u64>) {
    self.blocks += 1;
    if channels == 0 {
      return;
    }
    let now = Instant::now();
    // One look per block, for the chain and its sidetone tap.
    let owner_live = self.chain.shm_header().is_some_and(|h| self.owner.live(h, now));
    // Sidetone first, from the untouched input; it has its own failure flag.
    if let Some(layout) = self.chain.shm_layout() {
      // This chain's own leveler, not the shared memory: every mic APO instance publishes there.
      // A failed chain no longer levels, so the sidetone goes back to the raw microphone.
      let leveler_gain_db = if self.failed { 0.0 } else { self.chain.leveler_gain_db() };
      self.sidetone.process(layout, buf, channels, qpc_hns, leveler_gain_db, owner_live);
    }
    if self.failed {
      return;
    }
    let frames = buf.len() / channels;
    if self.mono_in.len() < frames {
      // Only if the engine exceeds the preallocated 85 ms period.
      self.mono_in.resize(frames, 0.0);
      self.mono_out.resize(frames, 0.0);
    }
    let qpc_hns = self.clock.next(qpc_hns, frames as u64);
    // With several mic chains (MFX: one per processing mode in use) the app shows one of them.
    let publishes = self.count.publishes();
    self.chain.set_publishing(publishes, MIC_CHAINS.load(Ordering::Relaxed) > 1);
    self.timing.note(now, frames, self.clock.rate, self.chain.shm_header().filter(|_| publishes));
    // Nobody's settings to apply (app not running for the user at the console): the raw mic.
    let Self { chain, mono_in, mono_out, bridge, failed, follow_owner, was_live, processed, .. } = self;
    if *follow_owner && !owner_live {
      if std::mem::replace(was_live, false) {
        chain.pause();
      }
      return;
    }
    if !std::mem::replace(was_live, true)
      && let Some(bridge) = bridge.as_mut()
    {
      bridge.reset();
    }
    let (Some(input), Some(output)) = (mono_in.get_mut(..frames), mono_out.get_mut(..frames)) else {
      return;
    };
    for (dst, frame) in input.iter_mut().zip(buf.chunks_exact(channels)) {
      *dst = frame.first().copied().unwrap_or(0.0);
    }
    let ran = catch_unwind(AssertUnwindSafe(|| match bridge {
      // Other rate: the bridge resamples to 48 kHz, runs the chain, and writes back into `input`.
      Some(bridge) => {
        let resampled = bridge.process(input, qpc_hns, |chain_in, chain_out, time| {
          chain.process_timed(&[chain_in], &mut [chain_out], time);
        });
        // The sinc filter rings slightly past the chain's limiter ceiling: keep it at full scale.
        for (dst, &src) in output.iter_mut().zip(input.iter()) {
          *dst = src.clamp(-1.0, 1.0);
        }
        resampled.is_ok()
      }
      None => {
        chain.process_timed(&[&*input], &mut [&mut *output], qpc_hns);
        true
      }
    }));
    if !matches!(ran, Ok(true)) {
      // Panic in the chain, or a resampler buffer mismatch (a bug): pass through from now on.
      *failed = true;
      return;
    }
    *processed += 1;
    for (frame, &sample) in buf.chunks_exact_mut(channels).zip(output.iter()) {
      frame.fill(sample);
    }
  }
}

impl Drop for MicProcessor {
  fn drop(&mut self) {
    if let Some(layout) = self.chain.shm_layout() {
      self.sidetone.release(layout);
    }
  }
}

impl SidetoneTap {
  /// `instance`: the APO's `instance_id`, unique in this process (ring owner = process id + it).
  fn new(rate: u32, instance: u32) -> Self {
    Self {
      chain: Box::new(SidetoneChain::new(rate as f32)),
      eq_version: None,
      denoiser: (rate == sidetone_denoise::SAMPLE_RATE).then(|| Box::new(SidetoneDenoiser::new())),
      rate,
      endpoint_hash: 0,
      instance,
      owner: None,
      // ~85 ms at 48 kHz; grows only if the engine sends longer blocks.
      mono: vec![0.0; 4096],
      peak: 0.0,
      blocks: 0,
      failed: false,
    }
  }

  /// `buf`: the interleaved raw input of this block. Never changed.
  /// `owner_live`: the mic APO's `OwnerWatch` for this block.
  fn process(
    &mut self,
    layout: &ShmLayout,
    buf: &[f32],
    channels: usize,
    qpc_hns: Option<u64>,
    leveler_gain_db: f32,
    owner_live: bool,
  ) {
    if self.failed || channels == 0 {
      return;
    }
    let inner = || self.process_inner(layout, buf, channels, qpc_hns, leveler_gain_db, owner_live);
    if catch_unwind(AssertUnwindSafe(inner)).is_err() {
      self.failed = true;
      self.release(layout);
    }
  }

  fn process_inner(
    &mut self,
    layout: &ShmLayout,
    buf: &[f32],
    channels: usize,
    qpc_hns: Option<u64>,
    leveler_gain_db: f32,
    owner_live: bool,
  ) {
    let h = &layout.header;
    let r = Ordering::Relaxed;
    let pair_hash = h.sidetone_mic_hash.load(r);
    // Reported only by the instance that is the pair's microphone or cannot tell: the APO of
    // another microphone would overwrite it.
    if self.endpoint_hash == pair_hash || self.endpoint_hash == 0 {
      h.sidetone_mic_hash_seen.store(self.endpoint_hash, r);
    }
    // The pair's microphone. An endpoint we could not identify may write too: a wrong
    // microphone is only a wrong voice, and the ring takes one writer.
    let ours = pair_hash != 0 && (self.endpoint_hash == pair_hash || self.endpoint_hash == 0);
    let mut settings = h.read_sidetone();
    settings.enabled &= ours && owner_live && !h.master_bypass.load(r);
    self.chain.configure(&settings, leveler_gain_db, h.mic_muted.load(r));
    let eq_version = h.sidetone_eq_version.load(Ordering::Acquire);
    if self.eq_version != Some(eq_version) {
      self.eq_version = Some(eq_version);
      self.chain.set_eq(h.read_sidetone_eq());
    }
    if self.chain.is_silent() {
      // Off, muted or not our pair: give the ring up (a clean stop for the reader), and tell
      // the suppressors about the pause, so nothing from before it is played afterwards.
      self.release(layout);
      if let Some(denoiser) = self.denoiser.as_mut() {
        denoiser.process(NoiseMode::Off, 0.0, &mut []);
      }
      return;
    }
    if self.owner.is_none() {
      // SAFETY: no preconditions.
      self.owner = layout.sidetone.claim_writer(unsafe { GetCurrentProcessId() }, self.instance);
      if self.owner.is_some() {
        // Filter memory and levels are from before the pause; the level fades in from zero.
        self.chain.reset();
      }
    }
    let Some(owner) = self.owner else {
      return; // another mic APO instance holds the ring
    };

    let frames = buf.len() / channels;
    if self.mono.len() < frames {
      self.mono.resize(frames, 0.0);
    }
    let Some(mono) = self.mono.get_mut(..frames) else {
      return;
    };
    for (dst, frame) in mono.iter_mut().zip(buf.chunks_exact(channels)) {
      *dst = frame.first().copied().unwrap_or(0.0);
    }
    self.chain.run_eq(mono);
    let wanted = self.chain.noise_mode();
    let mut active = if wanted == NoiseMode::Off { NoiseMode::Off } else { NoiseMode::Expander };
    if let Some(denoiser) = self.denoiser.as_mut() {
      if wanted.is_framed() {
        active = denoiser.process(wanted, settings.gate_range_db, mono);
      } else {
        // Tells it about the pause, so the next frame-based run starts fresh.
        denoiser.process(wanted, 0.0, &mut []);
      }
    }
    if active == NoiseMode::Expander {
      self.chain.expander_mut().process(mono);
    }
    self.chain.run_level(mono);
    let end_qpc = qpc_hns.map_or(0, |t| t.saturating_add(frames_to_hns(frames as u64, self.rate).max(0) as u64));
    if !layout.sidetone.write(owner, mono, self.rate, end_qpc) {
      // Another instance took the ring over (we were not called for a while): claim again.
      self.owner = None;
      return;
    }

    h.sidetone_writer_beat.fetch_add(1, r);
    h.sidetone_mic_rate.store(self.rate, r);
    h.sidetone_mic_block_frames.store(frames as u32, r);
    self.peak = mono.iter().fold(self.peak, |p, s| p.max(s.abs()));
    self.blocks += 1;
    if self.blocks >= SIDETONE_TELEMETRY_BLOCKS {
      h.sidetone_peak_permille.store((self.peak * 1000.0) as u32, r);
      let gate_db = if active == NoiseMode::Expander { self.chain.expander_mut().gain_db() } else { 0.0 };
      h.sidetone_gate_gain_db_x10.store((gate_db * 10.0) as i32, r);
      h.sidetone_noise_mode_active.store(active.to_u32(), r);
      let delay = self.denoiser.as_ref().map_or(0, |d| d.delay_samples(active)) as u64;
      h.sidetone_noise_delay_us.store((delay * 1_000_000 / u64::from(sidetone_denoise::SAMPLE_RATE)) as u32, r);
      (self.peak, self.blocks) = (0.0, 0);
    }
  }

  fn release(&mut self, layout: &ShmLayout) {
    if let Some(owner) = self.owner.take() {
      layout.sidetone.release_writer(owner);
    }
  }
}

impl CallTiming {
  /// One engine call at `now` for a block of `frames` at `rate` Hz.
  fn note(&mut self, now: Instant, frames: usize, rate: u32, h: Option<&ShmHeader>) {
    let block_us = frames as u64 * 1_000_000 / u64::from(rate.max(1));
    if let Some(last) = self.last.replace(now) {
      let interval_us = u64::try_from(now.duration_since(last).as_micros()).unwrap_or(u64::MAX);
      self.jitter_peak_us = self.jitter_peak_us.max(interval_us.abs_diff(block_us));
      if interval_us.saturating_mul(2) > block_us.saturating_mul(3)
        && let Some(h) = h
      {
        h.mic_call_gaps.fetch_add(1, Ordering::Relaxed);
      }
    }
    self.calls += 1;
    if self.calls >= CALL_TIMING_WINDOW_CALLS {
      if let Some(h) = h {
        h.mic_call_jitter_peak_us.store(u32::try_from(self.jitter_peak_us).unwrap_or(u32::MAX), Ordering::Relaxed);
      }
      self.calls = 0;
      self.jitter_peak_us = 0;
    }
  }
}

impl MicClock {
  fn new(rate: u32) -> Self {
    Self { rate, anchor: None }
  }

  /// Smoothed time of this block's first frame; `raw` is its measured time.
  fn next(&mut self, raw: Option<u64>, frames: u64) -> Option<u64> {
    let raw = raw.and_then(|t| i64::try_from(t).ok());
    let frames_to_hns = |n| frames_to_hns(n, self.rate);
    let estimate = match (self.anchor, raw) {
      (Some((t0, n)), Some(raw)) => {
        let expected = t0 + frames_to_hns(n);
        let error = raw - expected;
        if error.abs() > CLOCK_RESYNC_HNS {
          self.anchor = Some((raw, 0));
          raw
        } else {
          let t0 = t0 + error / CLOCK_CORRECTION_DIVISOR;
          self.anchor = Some((t0, n));
          t0 + frames_to_hns(n)
        }
      }
      // No measurement this block: keep counting.
      (Some((t0, n)), None) => t0 + frames_to_hns(n),
      (None, Some(raw)) => {
        self.anchor = Some((raw, 0));
        raw
      }
      (None, None) => return None,
    };
    if let Some((t0, n)) = self.anchor {
      self.anchor = Some((t0, n + frames));
    }
    u64::try_from(estimate).ok()
  }
}

fn frames_to_hns(frames: u64, rate: u32) -> i64 {
  (frames as i128 * HNS_PER_SEC / rate.max(1) as i128) as i64
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

/// Endpoint `{guid}` (lowercased) from the init block. Reads `pAPOEndpointProperties`
/// (`PKEY_AudioEndpoint_GUID`; same offset in every APOInitSystemEffects version), falling back
/// to `device_id`, the block's device collection entry (`endpoint_device_id`).
///
/// # Safety
/// `data` must point to `size` readable bytes or be null (audio engine contract).
unsafe fn endpoint_guid_from_init(size: u32, data: *const u8, device_id: Option<&str>) -> Option<String> {
  // SAFETY: the contract above; both sizes are checked before a field is read, reads are unaligned, and the property
  // store is borrowed (no Release); the string from `PropVariantToStringAlloc` is freed here.
  unsafe {
    if data.is_null() || (size as usize) < size_of::<APOInitSystemEffects>() {
      return None;
    }
    let init = data as *const APOInitSystemEffects;
    let declared = std::ptr::addr_of!((*init).APOInit.cbSize).read_unaligned();
    if (declared as usize) < size_of::<APOInitSystemEffects>() {
      return None;
    }
    // Field reads only: `ManuallyDrop` borrows COM pointers without taking ownership or calling Release.
    let props = std::ptr::addr_of!((*init).pAPOEndpointProperties).read_unaligned();
    if let Some(store) = props.as_ref()
      && let Ok(val) = store.GetValue(&PKEY_AUDIO_ENDPOINT_GUID)
      && let Ok(pwstr) = PropVariantToStringAlloc(&val)
    {
      let text = pwstr.to_string().ok();
      CoTaskMemFree(Some(pwstr.as_ptr() as *const c_void));
      if let Some(guid) = text.and_then(|s| normalize_endpoint_guid(&s)) {
        return Some(guid);
      }
    }
    device_id.and_then(normalize_endpoint_guid)
  }
}

fn normalize_endpoint_guid(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  let guid = trimmed.rsplit('.').next().unwrap_or(trimmed).trim();
  (guid.starts_with('{') && guid.ends_with('}') && guid.len() > 2).then(|| guid.to_ascii_lowercase())
}

/// `AudioProcessingMode` of an APOInitSystemEffects2 block (or a newer one, which extends it).
///
/// # Safety
/// `data` must point to `size` readable bytes or be null (audio engine contract).
unsafe fn processing_mode(size: u32, data: *const u8) -> Option<u128> {
  // SAFETY: the contract above; both sizes are checked before the field is read, unaligned.
  unsafe {
    if data.is_null() || (size as usize) < size_of::<APOInitSystemEffects2>() {
      return None;
    }
    let init = data as *const APOInitSystemEffects2;
    // Field reads only: the struct holds COM pointers we must not take ownership of.
    let declared = std::ptr::addr_of!((*init).APOInit.cbSize).read_unaligned();
    if (declared as usize) < size_of::<APOInitSystemEffects2>() {
      return None;
    }
    Some(std::ptr::addr_of!((*init).AudioProcessingMode).read_unaligned().to_u128())
  }
}

/// Device id of the endpoint the engine creates this APO for: the last device in the init
/// block's collection (documented for APOInitSystemEffects2, and how Equalizer APO finds its
/// endpoint). None when the block is neither APOInitSystemEffects nor APOInitSystemEffects2:
/// APOInitSystemEffects3 has `pDeviceCollection` at another offset, where the v1 layout would
/// read a garbage pointer inside audiodg.
///
/// # Safety
/// `data` must point to `size` readable bytes or be null (audio engine contract).
unsafe fn endpoint_device_id(size: u32, data: *const u8) -> Option<String> {
  // SAFETY: the contract above; the declared size must be exactly v1 or v2, where `pDeviceCollection` has this
  // offset; the collection is borrowed (no Release) and the id string freed here.
  unsafe {
    let (v1, v2) = (size_of::<APOInitSystemEffects>(), size_of::<APOInitSystemEffects2>());
    if data.is_null() || (size as usize) < v1 {
      return None;
    }
    let init = data as *const APOInitSystemEffects;
    let declared = addr_of!((*init).APOInit.cbSize).read_unaligned() as usize;
    if declared != v1 && declared != v2 {
      return None;
    }
    // Borrowed: the engine owns the collection; no AddRef, no Release.
    let raw = addr_of!((*init).pDeviceCollection).cast::<*mut c_void>().read_unaligned();
    let collection = IMMDeviceCollection::from_raw_borrowed(&raw)?;
    let last = collection.GetCount().ok()?.checked_sub(1)?;
    let device = collection.Item(last).ok()?;
    let id = device.GetId().ok()?;
    let text = id.to_string().ok();
    CoTaskMemFree(Some(id.as_ptr() as *const c_void));
    text
  }
}

fn interface_ids() -> [GUID; 4] {
  [
    IAudioProcessingObject::IID,
    IAudioProcessingObjectRT::IID,
    IAudioProcessingObjectConfiguration::IID,
    IAudioSystemEffects::IID,
  ]
}

/// CoTaskMemAlloc'ed properties; APO_REG_PROPERTIES ends in a variable-length IID array.
fn registration_properties(kind: ApoKind) -> Result<*mut APO_REG_PROPERTIES> {
  let ids = interface_ids();
  let size = size_of::<APO_REG_PROPERTIES>() + ids.len().saturating_sub(1) * size_of::<GUID>();
  // SAFETY: plain allocation; the caller (audio engine) frees it with CoTaskMemFree.
  let ptr = unsafe { CoTaskMemAlloc(size) } as *mut APO_REG_PROPERTIES;
  if ptr.is_null() {
    return Err(E_OUTOFMEMORY.into());
  }

  let mut props = APO_REG_PROPERTIES {
    clsid: kind.clsid(),
    Flags: APO_FLAG(APO_FLAG_DEFAULT.0 | APO_FLAG_INPLACE.0),
    u32MajorVersion: 1,
    u32MinorVersion: 0,
    u32MinInputConnections: 1,
    u32MaxInputConnections: 1,
    u32MinOutputConnections: 1,
    u32MaxOutputConnections: 1,
    u32MaxInstances: u32::MAX,
    u32NumAPOInterfaces: ids.len() as u32,
    ..Default::default()
  };
  copy_wide(&mut props.szFriendlyName, kind.friendly_name());
  copy_wide(&mut props.szCopyrightInfo, COPYRIGHT);

  // SAFETY: ptr has room for the struct plus ids.len() - 1 extra GUIDs (size above).
  unsafe {
    ptr.write(props);
    let list = addr_of_mut!((*ptr).iidAPOInterfaceList) as *mut GUID;
    for (i, id) in ids.iter().enumerate() {
      list.add(i).write(*id);
    }
  }
  Ok(ptr)
}

/// Copies `text` as a NUL-terminated UTF-16 string, truncating to fit.
fn copy_wide(dst: &mut [u16], text: &str) {
  let room = dst.len().saturating_sub(1);
  for (slot, unit) in dst.iter_mut().zip(text.encode_utf16().take(room).chain(std::iter::repeat(0))) {
    *slot = unit;
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
unsafe fn process_connection(
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

/// QPC time (100 ns units) of the first frame: the engine's V2 timestamp when present and
/// plausible, else "now minus the buffer length" (the delay estimator absorbs the offset).
///
/// A raw pointer, not a reference: a `&APO_CONNECTION_PROPERTY` covers only the V1 fields, so the
/// V2 timestamp behind them may not be read through it.
///
/// # Safety
/// `input` points to a live connection property, a whole `APO_CONNECTION_PROPERTY_V2` when its
/// signature says so (audio engine contract).
unsafe fn capture_time_hns(input: *const APO_CONNECTION_PROPERTY, rate: u32) -> Option<u64> {
  let now = now_hns()?;
  // SAFETY: V1 fields of a live property (contract above).
  let (signature, frames) = unsafe { ((*input).u32Signature, (*input).u32ValidFrameCount) };
  if signature == CONNECTION_V2_SIGNATURE {
    // SAFETY: the V2 signature means the engine passed a whole V2 property (contract above).
    let t = unsafe { addr_of!((*input.cast::<APO_CONNECTION_PROPERTY_V2>()).u64QPCTime).read() };
    if t.abs_diff(now) < MAX_TIMESTAMP_SKEW_HNS {
      return Some(t);
    }
  }
  // 0 = not locked yet (the engine doesn't process then); assume the usual rate.
  let rate = if rate == 0 { REF_SAMPLE_RATE } else { rate };
  let duration = frames_to_hns(u64::from(frames), rate).max(0) as u64;
  Some(now.saturating_sub(duration))
}

fn now_hns() -> Option<u64> {
  let (mut counter, mut freq) = (0i64, 0i64);
  // SAFETY: out parameters are locals.
  unsafe {
    QueryPerformanceCounter(&mut counter).ok()?;
    QueryPerformanceFrequency(&mut freq).ok()?;
  }
  if freq <= 0 {
    return None;
  }
  u64::try_from(counter as i128 * HNS_PER_SEC / freq as i128).ok()
}

/// Next sidetone block (mono, `frames` long) into `block`. True = there is audio to mix in.
fn read_sidetone(
  reader: &mut SidetoneReader,
  block: &mut Vec<f32>,
  layout: &ShmLayout,
  frames: usize,
  rate: f32,
  endpoint: u32,
) -> bool {
  let h = &layout.header;
  let r = Ordering::Relaxed;
  let pair = h.sidetone_output_hash.load(r);
  // Reported only by the instance that is the pair's output or cannot tell: the APO of
  // another output would overwrite it.
  if endpoint == pair || endpoint == 0 {
    h.sidetone_output_hash_seen.store(endpoint, r);
  }
  // Only on the pair's output, and only when this endpoint is known to be it: sidetone in
  // speakers is a feedback loop.
  if !h.sidetone_enabled.load(r) || pair == 0 || pair != endpoint {
    reader.pause();
    return false;
  }
  if block.len() < frames {
    block.resize(frames, 0.0);
  }
  let Some(out) = block.get_mut(..frames) else {
    return false;
  };
  let info = reader.read(&layout.sidetone, out, rate);
  h.sidetone_out_rate.store(rate as u32, r);
  h.sidetone_out_block_frames.store(frames as u32, r);
  h.sidetone_underruns.store(reader.underruns(), r);
  h.sidetone_resyncs.store(reader.resyncs(), r);
  if !info.playing {
    return false;
  }
  h.sidetone_reader_beat.fetch_add(1, r);
  h.sidetone_queued_us.store((u64::from(info.queued) * 1_000_000 / u64::from(info.ring_rate.max(1))) as u32, r);
  // Capture to mix; the units are 100 ns.
  let age_us = now_hns()
    .zip(layout.sidetone.capture_qpc_hns(info.queued))
    .map_or(0, |(now, captured)| now.saturating_sub(captured) / 10);
  h.sidetone_age_us.store(u32::try_from(age_us).unwrap_or(u32::MAX), r);
  true
}

/// Adds the mono sidetone to the first two channels (left, right) of an interleaved buffer.
fn add_sidetone(buf: &mut [f32], channels: usize, mono: &[f32]) {
  for (frame, sample) in buf.chunks_exact_mut(channels.max(1)).zip(mono) {
    frame.iter_mut().take(2).for_each(|channel| *channel += sample);
  }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
