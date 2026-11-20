//! Windows announces every new audio session on the outputs we watch (`IAudioSessionNotification`),
//! so the idle service sees an app that starts playing without walking every session each second
//! (`apply_session_mix`). The callback only sets a flag; the service loop reads it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
  IAudioSessionControl, IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
};
use windows::core::implement;
use windows_core::Ref;

#[implement(IAudioSessionNotification)]
struct NewSessionFlag(Arc<AtomicBool>);

/// The registrations on every active output, renewed by `renew`.
#[derive(Default)]
pub(super) struct SessionWatch {
  new_session: Arc<AtomicBool>,
  /// One callback object per renewal: the old one is unregistered after the new one is in, so
  /// no session goes unannounced in between and no registration is mistaken for another.
  client: Option<IAudioSessionNotification>,
  managers: Vec<IAudioSessionManager2>,
  /// Every active output announces its new sessions.
  watching: bool,
  renewed_at: Option<Instant>,
  /// The last outcome logged (outputs watched, or the error): the same one is not logged again.
  logged: Option<Result<usize, String>>,
}

impl IAudioSessionNotification_Impl for NewSessionFlag_Impl {
  // A thread of Windows' audio session manager: no Windows audio calls (Microsoft: none from
  // inside the notification), no locks, nothing that can fail.
  fn OnSessionCreated(&self, _new_session: Ref<IAudioSessionControl>) -> windows_core::Result<()> {
    self.0.store(true, Ordering::Relaxed);
    Ok(())
  }
}

impl SessionWatch {
  /// A session was created since the last call.
  pub(super) fn take_new_session(&self) -> bool {
    self.new_session.swap(false, Ordering::Relaxed)
  }

  /// Every active output announces its new sessions (else the caller keeps polling).
  pub(super) fn watching(&self) -> bool {
    self.watching
  }

  /// Never renewed, renewal asked for (`renew_soon`), or `interval` passed: an audio service
  /// restart drops the registrations without a word, and a failed one is retried this often.
  pub(super) fn renew_due(&self, interval: Duration) -> bool {
    self.renewed_at.is_none_or(|t| t.elapsed() >= interval)
  }

  /// The outputs changed: renew at the next pass.
  pub(super) fn renew_soon(&mut self) {
    self.renewed_at = None;
  }

  /// Registers on `managers` (every active output's), then drops the old registrations. Call on
  /// a thread with COM initialized as MTA: Windows sends session notifications only there.
  pub(super) fn renew(&mut self, managers: Vec<IAudioSessionManager2>) {
    self.renewed_at = Some(Instant::now());
    let client: IAudioSessionNotification = NewSessionFlag(Arc::clone(&self.new_session)).into();
    let mut registered = Vec::with_capacity(managers.len());
    let mut error = managers.is_empty().then(|| "no active output".to_string());
    for manager in managers {
      // SAFETY: COM calls on a manager the caller just activated and `client`, both alive here;
      // the manager is kept with the registration until `unregister`. The enumerator is asked
      // for first: without it the session manager sends no notifications.
      let registered_now =
        unsafe { manager.GetSessionEnumerator().and_then(|_| manager.RegisterSessionNotification(&client)) };
      match registered_now {
        Ok(()) => registered.push(manager),
        Err(e) => error = Some(e.to_string()),
      }
    }
    self.unregister();
    (self.client, self.managers) = (Some(client), registered);
    self.watching = error.is_none();
    let outcome = error.map_or(Ok(self.managers.len()), Err);
    if self.logged.as_ref() != Some(&outcome) {
      match &outcome {
        Ok(n) => log::info!("[ChatMix] new sessions announced by Windows on {n} outputs"),
        Err(e) => log::warn!("[ChatMix] new sessions not announced ({e}): looking for them every second"),
      }
      self.logged = Some(outcome);
    }
  }

  fn unregister(&mut self) {
    let Some(client) = self.client.take() else { return };
    for manager in self.managers.drain(..) {
      // SAFETY: unregisters the callback `renew` registered on this manager, once.
      let _ = unsafe { manager.UnregisterSessionNotification(&client) };
    }
  }
}

impl Drop for SessionWatch {
  fn drop(&mut self) {
    self.unregister();
  }
}
