use windows::Win32::Media::Audio::ERole;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows_core::Interface;

pub const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
pub const IID_IPOLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

#[repr(C)]
#[allow(non_snake_case)]
pub struct IPolicyConfig_Vtbl {
  pub base: windows_core::IUnknown_Vtbl,
  pub GetMixFormat: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut std::ffi::c_void) -> HRESULT,
  pub GetDeviceFormat: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32, *mut std::ffi::c_void) -> HRESULT,
  pub ResetDeviceFormat: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR) -> HRESULT,
  pub SetDeviceFormat: unsafe extern "system" fn(
    *mut std::ffi::c_void,
    PCWSTR,
    *const std::ffi::c_void,
    *const std::ffi::c_void,
  ) -> HRESULT,
  pub GetProcessingPeriod: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32, *mut i64, *mut i64) -> HRESULT,
  pub SetProcessingPeriod: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *const i64) -> HRESULT,
  pub GetShareMode: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *mut std::ffi::c_void) -> HRESULT,
  pub SetShareMode: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *const std::ffi::c_void) -> HRESULT,
  pub GetPropertyValue:
    unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, *const std::ffi::c_void, *mut std::ffi::c_void) -> HRESULT,
  pub SetPropertyValue: unsafe extern "system" fn(
    *mut std::ffi::c_void,
    PCWSTR,
    *const std::ffi::c_void,
    *const std::ffi::c_void,
  ) -> HRESULT,
  pub SetDefaultEndpoint: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, ERole) -> HRESULT,
  pub SetEndpointVisibility: unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> HRESULT,
}

#[derive(Clone)]
#[repr(transparent)]
pub struct IPolicyConfig(pub windows_core::IUnknown);

unsafe impl windows_core::Interface for IPolicyConfig {
  type Vtable = IPolicyConfig_Vtbl;
  const IID: GUID = IID_IPOLICY_CONFIG;
}

impl IPolicyConfig {
  pub fn new() -> windows_core::Result<Self> {
    unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
  }

  pub fn set_default_endpoint(&self, device_id: PCWSTR, role: ERole) -> windows_core::Result<()> {
    unsafe {
      let vtbl = self.0.vtable() as *const _ as *const IPolicyConfig_Vtbl;
      ((*vtbl).SetDefaultEndpoint)(self.0.as_raw(), device_id, role).ok()
    }
  }

  pub fn set_default_endpoint_all_roles(&self, device_id: PCWSTR) -> windows_core::Result<()> {
    use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia};
    self.set_default_endpoint(device_id, eConsole)?;
    self.set_default_endpoint(device_id, eMultimedia)?;
    self.set_default_endpoint(device_id, eCommunications)?;
    Ok(())
  }
}
