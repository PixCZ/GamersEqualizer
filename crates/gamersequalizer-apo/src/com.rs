//! The hand-written aggregatable COM object: vtables, IUnknown, the class factory, the DLL exports
//! and the registration properties.

use super::*;

const COPYRIGHT: &str = "GamersEqualizer";

pub(super) const OFF_APO: usize = offset_of!(ApoObject, apo);

pub(super) const OFF_RT: usize = offset_of!(ApoObject, rt);

pub(super) const OFF_CFG: usize = offset_of!(ApoObject, cfg);

const OFF_SFX: usize = offset_of!(ApoObject, sfx);

const OFF_INNER: usize = offset_of!(ApoObject, inner);

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

/// # Safety
/// Called by COM with pointers it owns; every pointer is null-checked before use.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
  rclsid: *const GUID,
  riid: *const GUID,
  ppv: *mut *mut c_void,
) -> HRESULT {
  guarded("DllGetClassObject", E_UNEXPECTED, || {
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
  })
}

/// Shortcut: never unload. audiodg keeps APOs alive anyway; tracking live objects for a
/// correct answer is not worth it until the DLL has to be upgraded without an audio restart.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
  S_FALSE
}

impl IClassFactory_Impl for ApoFactory_Impl {
  fn CreateInstance(&self, outer: Ref<'_, IUnknown>, riid: *const GUID, object: *mut *mut c_void) -> Result<()> {
    guarded("CreateInstance", Err(E_UNEXPECTED.into()), || {
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
    })
  }

  fn LockServer(&self, _lock: BOOL) -> Result<()> {
    Ok(())
  }
}

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
pub(super) unsafe fn state<'a, const OFF: usize>(this: *mut c_void) -> &'a ApoState {
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

fn interface_ids() -> [GUID; 4] {
  [
    IAudioProcessingObject::IID,
    IAudioProcessingObjectRT::IID,
    IAudioProcessingObjectConfiguration::IID,
    IAudioSystemEffects::IID,
  ]
}

/// CoTaskMemAlloc'ed properties; APO_REG_PROPERTIES ends in a variable-length IID array.
pub(super) fn registration_properties(kind: ApoKind) -> Result<*mut APO_REG_PROPERTIES> {
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
