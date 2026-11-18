pub mod aec_delay_cache;
pub mod aec_reference;
pub mod apo_install;
pub mod apo_link;
pub mod app_channels;
pub mod chain_log;
pub mod chatmix;
pub mod console_owner;
pub mod device_ids;
pub mod dolby_atmos;
pub mod dsp_watch;
pub mod ducking;
pub mod endpoint_lock;
pub mod game_detector;
pub mod leveler_log;
pub mod mic_gain;
pub mod mic_probe;
pub mod mmdevice;
pub mod policy_config;
pub mod shell;
pub mod sidetone;
pub mod stream_router;
pub mod worker;

pub use chatmix::{AudioBus, AudioSessionInfo, ChatMixController};
pub use dolby_atmos::{DolbyAtmosGuard, SpatialAudioStatus, SpatialFormat};
pub use endpoint_lock::{AudioEndpointInfo, EndpointLockManager, EndpointLockState};
pub use game_detector::{GameDetector, GameForegroundEvent};

use std::sync::Arc;

pub struct WinAudioSystem {
  pub endpoint_lock: EndpointLockManager,
  pub dolby_guard: DolbyAtmosGuard,
  pub chatmix: ChatMixController,
  pub game_detector: Option<GameDetector>,
}

impl WinAudioSystem {
  pub fn new() -> Result<Self, String> {
    let state = Arc::new(EndpointLockState::default());
    let endpoint_lock = EndpointLockManager::new(state).map_err(|e| e.to_string())?;
    let dolby_guard = DolbyAtmosGuard::default();
    let mut chatmix = ChatMixController::new();
    let game_detector = GameDetector::start().ok();

    let devices = endpoint_lock.enumerate_playback_devices();
    let def_id = endpoint_lock.get_default_output_id();
    chatmix.bind_endpoints(&devices, def_id.as_deref());
    chatmix.use_mic(None);

    Ok(Self { endpoint_lock, dolby_guard, chatmix, game_detector })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_win_audio_init() {
    let sys = WinAudioSystem::new();
    println!("WinAudioSystem::new() is_ok: {:?}", sys.is_ok());
    if let Err(ref e) = sys {
      println!("WinAudioSystem err: {}", e);
    }
    assert!(sys.is_ok());
    if let Ok(ref s) = sys {
      println!("Bound Master endpoint: {:?}", s.chatmix.master_device_id);
      println!("Bound Game endpoint:   {:?}", s.chatmix.game_device_id);
      println!("Bound Chat endpoint:   {:?}", s.chatmix.chat_device_id);
      println!("Bound Media endpoint:  {:?}", s.chatmix.media_device_id);
      println!("Channel volumes:");
      println!("  Master: {:.1}%", s.chatmix.master.volume * 100.0);
      println!("  Game:   {:.1}%", s.chatmix.game.volume * 100.0);
      println!("  Chat:   {:.1}%", s.chatmix.chat.volume * 100.0);
      println!("  Media:  {:.1}%", s.chatmix.media.volume * 100.0);
      println!("  Mic:    {:.1}%", s.chatmix.mic.volume * 100.0);
    }
  }
}
