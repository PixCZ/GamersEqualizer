use crate::plugin::{GamersEqualizerPlugin, PluginMode};
use std::ffi::c_void;
use std::sync::Mutex;

pub const VST_MAGIC: i32 = 0x56737450; // 'VstP'
pub const GAMERSEQUALIZER_VST_ID: i32 = 0x474D4551; // 'GMEQ'

pub const EFF_FLAGS_CAN_REPLACING: i32 = 1 << 4;

pub type AudioMasterCallback = unsafe extern "C" fn(
  effect: *mut AEffect,
  opcode: i32,
  index: i32,
  value: isize,
  ptr: *mut c_void,
  opt: f32,
) -> isize;

#[repr(C)]
pub struct AEffect {
  pub magic: i32,
  pub dispatcher: unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize,
  pub process: unsafe extern "C" fn(*mut AEffect, *const *const f32, *const *mut f32, i32),
  pub set_parameter: unsafe extern "C" fn(*mut AEffect, i32, f32),
  pub get_parameter: unsafe extern "C" fn(*mut AEffect, i32) -> f32,

  pub num_programs: i32,
  pub num_params: i32,
  pub num_inputs: i32,
  pub num_outputs: i32,

  pub flags: i32,
  pub resvd1: isize,
  pub resvd2: isize,
  pub initial_delay: i32,
  pub real_qualities: i32,
  pub off_qualities: i32,
  pub io_ratio: f32,
  pub object: *mut c_void,
  pub user: *mut c_void,
  pub unique_id: i32,
  pub version: i32,

  pub process_replacing: unsafe extern "C" fn(*mut AEffect, *const *const f32, *const *mut f32, i32),
  pub process_double_replacing: *mut c_void,
}

struct PluginContainer {
  plugin: Mutex<GamersEqualizerPlugin>,
}

pub unsafe extern "C" fn dispatcher(
  effect: *mut AEffect,
  opcode: i32,
  _index: i32,
  _value: isize,
  _ptr: *mut c_void,
  opt: f32,
) -> isize {
  match opcode {
    0 => 0, // effOpen
    1 => {
      // effClose
      if !effect.is_null() && !(*effect).object.is_null() {
        let _ = Box::from_raw((*effect).object as *mut PluginContainer);
        (*effect).object = std::ptr::null_mut();
        let _ = Box::from_raw(effect);
      }
      0
    }
    10 => {
      // effSetSampleRate
      if !effect.is_null() && !(*effect).object.is_null() {
        let container = &*((*effect).object as *const PluginContainer);
        if let Ok(mut plug) = container.plugin.lock() {
          *plug = GamersEqualizerPlugin::new(opt);
        }
      }
      0
    }
    _ => 0,
  }
}

pub unsafe extern "C" fn set_parameter(effect: *mut AEffect, index: i32, value: f32) {
  if effect.is_null() || (*effect).object.is_null() {
    return;
  }
  let container = &*((*effect).object as *const PluginContainer);
  if let Ok(mut plug) = container.plugin.lock() {
    match index {
      0 => {
        // Mode: 0.0 = Auto, 0.5 = SpeakerTap, 1.0 = MicProcessor
        if value < 0.33 {
          plug.mode = PluginMode::Auto;
        } else if value < 0.66 {
          plug.mode = PluginMode::SpeakerTap;
        } else {
          plug.mode = PluginMode::MicProcessor;
        }
      }
      _ => {}
    }
  }
}

pub unsafe extern "C" fn get_parameter(effect: *mut AEffect, index: i32) -> f32 {
  if effect.is_null() || (*effect).object.is_null() {
    return 0.0;
  }
  let container = &*((*effect).object as *const PluginContainer);
  if let Ok(plug) = container.plugin.lock() {
    match index {
      0 => match plug.mode {
        PluginMode::Auto => 0.0,
        PluginMode::SpeakerTap => 0.5,
        PluginMode::MicProcessor => 1.0,
      },
      _ => 0.0,
    }
  } else {
    0.0
  }
}

pub unsafe extern "C" fn process(
  effect: *mut AEffect,
  inputs: *const *const f32,
  outputs: *const *mut f32,
  sample_frames: i32,
) {
  process_replacing(effect, inputs, outputs, sample_frames);
}

pub unsafe extern "C" fn process_replacing(
  effect: *mut AEffect,
  inputs: *const *const f32,
  outputs: *const *mut f32,
  sample_frames: i32,
) {
  if effect.is_null() || (*effect).object.is_null() || sample_frames <= 0 {
    return;
  }
  let container = &*((*effect).object as *const PluginContainer);
  if let Ok(mut plug) = container.plugin.lock() {
    let in_count = (*effect).num_inputs as usize;
    let out_count = (*effect).num_outputs as usize;
    let frames = sample_frames as usize;

    let in_slices: Vec<&[f32]> = (0..in_count).map(|i| std::slice::from_raw_parts(*inputs.add(i), frames)).collect();

    let mut out_slices: Vec<&mut [f32]> =
      (0..out_count).map(|i| std::slice::from_raw_parts_mut(*outputs.add(i), frames)).collect();

    plug.process(&in_slices, &mut out_slices);
  }
}

#[no_mangle]
pub unsafe extern "C" fn VSTPluginMain(_audio_master: AudioMasterCallback) -> *mut AEffect {
  let container = Box::new(PluginContainer { plugin: Mutex::new(GamersEqualizerPlugin::new(48000.0)) });

  let effect = Box::new(AEffect {
    magic: VST_MAGIC,
    dispatcher,
    process,
    set_parameter,
    get_parameter,
    num_programs: 1,
    num_params: 3,
    num_inputs: 2,
    num_outputs: 2,
    flags: EFF_FLAGS_CAN_REPLACING,
    resvd1: 0,
    resvd2: 0,
    initial_delay: 0,
    real_qualities: 0,
    off_qualities: 0,
    io_ratio: 1.0,
    object: Box::into_raw(container) as *mut c_void,
    user: std::ptr::null_mut(),
    unique_id: GAMERSEQUALIZER_VST_ID,
    version: 1,
    process_replacing,
    process_double_replacing: std::ptr::null_mut(),
  });

  Box::into_raw(effect)
}
