//! The microphone and the output the app works with, for the service's threads that do not own
//! `ChatMixController` (sidetone, echo reference, APO link). The service loop resolves both in
//! one place (`ChatMixController::use_mic`, `chatmix::master_endpoint_id`: the pick while it is
//! connected, else the Windows default, a virtual default replaced by a physical device) and
//! publishes them here on every pass. No thread asks Windows for "the default" itself: it would
//! end up on another device than the mixer whenever the two rules differ.

use std::sync::RwLock;

/// (microphone id, output id); None until the service loop has bound its devices.
static IN_USE: RwLock<(Option<String>, Option<String>)> = RwLock::new((None, None));

/// From the service loop, every pass: `clone_from` reuses the text, so an unchanged id costs
/// a compare-sized copy and no allocation.
pub fn publish(mic: &Option<String>, output: &Option<String>) {
  let mut in_use = IN_USE.write().unwrap_or_else(|e| e.into_inner());
  in_use.0.clone_from(mic);
  in_use.1.clone_from(output);
}

pub fn mic() -> Option<String> {
  IN_USE.read().unwrap_or_else(|e| e.into_inner()).0.clone()
}

pub fn output() -> Option<String> {
  IN_USE.read().unwrap_or_else(|e| e.into_inner()).1.clone()
}
