//! What the engine's `APOInitSystemEffects*` block says: endpoint GUID, device id, processing mode.

use super::*;

/// `PKEY_AudioEndpoint_GUID` (`{1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E}, 4`): carried on
/// `APOInitSystemEffects.pAPOEndpointProperties` in Windows Audio Engine (`audiodg.exe`).
const PKEY_AUDIO_ENDPOINT_GUID: PROPERTYKEY =
  PROPERTYKEY { fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e), pid: 4 };

/// Endpoint `{guid}` (lowercased) from the init block. Reads `pAPOEndpointProperties`
/// (`PKEY_AudioEndpoint_GUID`; same offset in every APOInitSystemEffects version), falling back
/// to `device_id`, the block's device collection entry (`endpoint_device_id`).
///
/// # Safety
/// `data` must point to `size` readable bytes or be null (audio engine contract).
pub(super) unsafe fn endpoint_guid_from_init(size: u32, data: *const u8, device_id: Option<&str>) -> Option<String> {
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

pub(super) fn normalize_endpoint_guid(raw: &str) -> Option<String> {
  let trimmed = raw.trim();
  let guid = trimmed.rsplit('.').next().unwrap_or(trimmed).trim();
  (guid.starts_with('{') && guid.ends_with('}') && guid.len() > 2).then(|| guid.to_ascii_lowercase())
}

/// `AudioProcessingMode` of an APOInitSystemEffects2 block (or a newer one, which extends it).
///
/// # Safety
/// `data` must point to `size` readable bytes or be null (audio engine contract).
pub(super) unsafe fn processing_mode(size: u32, data: *const u8) -> Option<u128> {
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
pub(super) unsafe fn endpoint_device_id(size: u32, data: *const u8) -> Option<String> {
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
