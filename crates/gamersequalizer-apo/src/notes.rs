//! Notes to the Application log, and the guard around COM entry points that build something.

use super::*;

/// An Application log note scoped per endpoint GUID (or instance id fallback when Windows
/// did not pass one): same format on multiple devices never suppresses another device's notes.
pub(super) fn note_for(st: &ApoState, level: Level, msg: String) {
  note_tagged(st.kind, st.endpoint_tag.get().map(String::as_str), st.instance_id, level, msg);
}

pub(super) fn note_tagged(kind: ApoKind, endpoint_tag: Option<&str>, instance_id: u32, level: Level, msg: String) {
  let prefix = match endpoint_tag {
    Some(tag) => format!("{} [{tag}]", kind.friendly_name()),
    None => kind.friendly_name().to_string(),
  };
  let text = format!("{prefix}: {msg}");
  let key = if endpoint_tag.is_some() { text.clone() } else { format!("#{instance_id}:{text}") };
  event_log::note(level, &key, &text);
}

/// Runs a COM entry point that builds something (class, instance, init, stream): a panic in it
/// returns `on_panic` and leaves a note, where unwinding out of `extern "system"` would abort
/// audiodg and every sound on the system with it.
pub(super) fn guarded<T>(entry: &str, on_panic: T, f: impl FnOnce() -> T) -> T {
  catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
    note(Level::Warning, format!("{entry} panicked; Windows goes on without this effect"));
    on_panic
  })
}

/// An Application log note keyed by its own text: each distinct text once per process.
pub(super) fn note(level: Level, text: String) {
  event_log::note(level, &text, &text);
}
