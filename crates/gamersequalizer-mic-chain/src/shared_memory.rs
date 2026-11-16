use gamersequalizer_common::shm::{SHM_MAGIC, SHM_NAME, ShmLayout, ShmRingBuffer};
use std::ptr::NonNull;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::BOOL;
use windows::core::PCWSTR;

pub struct SharedMemoryClient {
  handle: HANDLE,
  mapped_ptr: NonNull<ShmLayout>,
}

// SAFETY: the mapping is process-wide memory accessed through atomics; the handle may be
// closed from any thread.
unsafe impl Send for SharedMemoryClient {}

impl SharedMemoryClient {
  /// The daemon's per-session mapping (`SHM_NAME`).
  pub fn open_or_create() -> Result<Self, String> {
    Self::create_named(SHM_NAME, None)
  }

  /// Create (or open if it exists) `name`. `sddl` sets the DACL of a newly created mapping,
  /// e.g. so audiodg (LOCAL SERVICE) and the logged-on user share a Global mapping.
  /// A fresh mapping gets default settings.
  pub fn create_named(name: &str, sddl: Option<&str>) -> Result<Self, String> {
    // SAFETY: the handle is a file mapping of the layout's size, new or one that was there.
    let mut client = unsafe {
      let handle = create_mapping(name, sddl, ShmLayout::total_size_bytes())?;
      Self::map(handle)?
    };
    // One that was there is in use by other processes: read through a shared reference, and
    // written (`layout_mut`) only while its magic is not set. Nothing locks that across
    // processes: two that make the mapping at the same instant both write the same defaults.
    if client.layout().header.magic != SHM_MAGIC {
      client.layout_mut().header.init_defaults();
    } else if let Some(e) = client.layout().header.layout_mismatch() {
      return Err(format!("{name} has {e}"));
    }
    Ok(client)
  }

  /// Open an existing mapping only (no create rights needed, e.g. a Global one from a user process).
  pub fn open_existing(name: &str) -> Result<Self, String> {
    let wide_name = wide(name);
    // SAFETY: NUL-terminated name owned by this frame.
    unsafe {
      let handle = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, PCWSTR(wide_name.as_ptr()))
        .map_err(|e| format!("OpenFileMappingW({name}) failed: {e}"))?;
      let client = Self::map(handle)?;
      let header = &client.layout().header;
      if header.magic != SHM_MAGIC {
        return Err(format!("{name} is not initialized"));
      }
      if let Some(e) = header.layout_mismatch() {
        return Err(format!("{name} has {e}"));
      }
      Ok(client)
    }
  }

  pub fn ring_buffer(&mut self) -> ShmRingBuffer<'_> {
    // SAFETY: `mapped_ptr` is a live view of a whole `ShmLayout` until `Drop`, and `&mut self` keeps
    // this client's references exclusive. Other processes share the memory: the header is atomics,
    // the frames have one writer per mapping (the service's loopback thread) and the reader
    // sanitizes torn frames (`read_frames_from`).
    unsafe { ShmRingBuffer::new(self.mapped_ptr.as_mut()) }
  }

  pub fn layout(&self) -> &ShmLayout {
    // SAFETY: as `ring_buffer`; a shared reference, so this process does not write through it.
    unsafe { self.mapped_ptr.as_ref() }
  }

  fn layout_mut(&mut self) -> &mut ShmLayout {
    // SAFETY: as `ring_buffer`; used only to set defaults while the mapping's magic is not set
    // (`create_named` says what that does not rule out).
    unsafe { self.mapped_ptr.as_mut() }
  }

  /// Takes ownership of `handle` (closed on failure).
  ///
  /// # Safety
  /// `handle` is a file mapping handle the caller owns and does not close itself.
  unsafe fn map(handle: HANDLE) -> Result<Self, String> {
    // SAFETY: the caller hands the handle over (contract above). The view is the layout's size or
    // the call fails; it and the handle are released once, in `Drop`, or the handle here on failure.
    unsafe {
      if handle.is_invalid() {
        return Err("file mapping handle is invalid".to_string());
      }
      let size = ShmLayout::total_size_bytes();
      let view = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size);
      let Some(mapped_ptr) = NonNull::new(view.Value as *mut ShmLayout) else {
        let _ = CloseHandle(handle);
        return Err("MapViewOfFile returned null pointer".to_string());
      };
      Ok(Self { handle, mapped_ptr })
    }
  }
}

impl Drop for SharedMemoryClient {
  fn drop(&mut self) {
    // SAFETY: the view and the handle come from `map` and are released only here.
    unsafe {
      let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
        Value: self.mapped_ptr.as_ptr() as *mut _,
      });
      let _ = CloseHandle(self.handle);
    }
  }
}

/// Create (or open) a pagefile-backed mapping of `size` bytes; `sddl` sets the DACL when created.
pub(crate) fn create_mapping(name: &str, sddl: Option<&str>, size: usize) -> Result<HANDLE, String> {
  let wide_name = wide(name);
  let size = u64::try_from(size).map_err(|_| "mapping too large".to_string())?;
  // SAFETY: Win32 calls with owned, NUL-terminated buffers; the descriptor is freed below.
  unsafe {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    if let Some(sddl) = sddl {
      let wide_sddl = wide(sddl);
      ConvertStringSecurityDescriptorToSecurityDescriptorW(
        PCWSTR(wide_sddl.as_ptr()),
        SDDL_REVISION_1,
        &mut descriptor,
        None,
      )
      .map_err(|e| format!("invalid SDDL: {e}"))?;
    }
    let attributes = SECURITY_ATTRIBUTES {
      nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
      lpSecurityDescriptor: descriptor.0,
      bInheritHandle: BOOL(0),
    };
    let result = CreateFileMappingW(
      INVALID_HANDLE_VALUE,
      sddl.map(|_| &attributes as *const SECURITY_ATTRIBUTES),
      PAGE_READWRITE,
      (size >> 32) as u32,
      size as u32,
      PCWSTR(wide_name.as_ptr()),
    );
    if !descriptor.0.is_null() {
      let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result.map_err(|e| format!("CreateFileMappingW({name}) failed: {e}"))
  }
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}
