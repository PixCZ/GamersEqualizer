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
//! after a panic the APO passes audio through unchanged. The COM entry points that build
//! something are wrapped too (`guarded`): a panic there fails that call with `E_UNEXPECTED`. The real-time path never blocks
//! (`try_lock` only).
//!
//! Settings, the speaker reference and AEC telemetry go through the Global mapping
//! `APO_SHM_NAME`, which this DLL creates (audiodg runs in session 0 as LOCAL SERVICE, which
//! may create Global objects; the user's daemon may not, so it opens it).
//!
//! The audio engine creates APOs *aggregated* (pUnkOuter != NULL) and refuses an APO that
//! returns CLASS_E_NOAGGREGATION, which disables the whole capture endpoint. windows-rs
//! `#[implement]` cannot be aggregated, so the APO object (`com.rs`) is a hand-written COM object:
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

mod clock;
mod com;
mod entry;
mod event_log;
mod init_block;
mod mic;
mod notes;
mod output;
mod rate_bridge;
mod sidetone_tap;
mod stream;

use event_log::Level;
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{addr_of, addr_of_mut, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use clock::*;
use com::*;
use entry::*;
use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::limiter::{PeakLimiter, soft_limit};
use gamersequalizer_common::shm::{APO_SHM_NAME, OwnerWatch, REF_SAMPLE_RATE, ShmHeader, ShmLayout};
use gamersequalizer_common::sidetone::{NoiseMode, SidetoneChain, SidetoneReader, device_hash};
use gamersequalizer_common::streams::{StreamBus, StreamInfo};
use gamersequalizer_mic_chain::sidetone_denoise::{self, SidetoneDenoiser};
use gamersequalizer_mic_chain::{MicChain, SharedMemoryClient};
use init_block::*;
use mic::*;
use notes::*;
use output::*;
use rate_bridge::RateBridge;
use sidetone_tap::*;
use stream::*;
use windows::Win32::Foundation::{
  CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER,
  E_UNEXPECTED, PROPERTYKEY, S_FALSE, S_OK,
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

/// Mic chain. Must match `$Clsid` (Capture) in `scripts/apo-install.ps1`.
pub const CLSID_MIC_APO: GUID = GUID::from_u128(0x17e8d42e_dc01_4bd0_90ff_ad7c83a5ddea);

/// Output EQ. Must match `$Clsid` (Render) in `scripts/apo-install.ps1`.
pub const CLSID_OUTPUT_EQ_APO: GUID = GUID::from_u128(0x6b0f3c1e_8f2a_4d7b_9a51_3e2c7d4b9f10);

/// Stream probe. Must match `$Clsid` (Render, -Slot SFX) in `scripts/apo-install.ps1`.
pub const CLSID_STREAM_APO: GUID = GUID::from_u128(0x3c9a5e21_7d4f_4b8e_a1c6_5f2e8d9b0a47);

/// Stream EQ crossfade when a stream changes channel (e.g. once it is paired).
const STREAM_FADE_SECONDS: f32 = 0.02;

#[cfg(test)]
mod tests;
