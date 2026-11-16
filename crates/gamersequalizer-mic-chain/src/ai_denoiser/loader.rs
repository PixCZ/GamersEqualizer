//! Loading a heavy model on a short-lived thread and handing it to the audio thread.

use super::*;

/// A heavy engine, or why it could not be loaded.
pub(super) enum Loaded {
  DeepFilter(Result<Box<DeepFilterEngine>, String>),
  Dpdf(Result<Box<DpdfEngine>, String>),
}

/// Carries a `Loaded` from the loader thread to the audio thread, once.
pub(super) struct HandOver(pub(super) Loaded);

// SAFETY: tract's run states hold `Rc`s, so the engines are not `Send`. Moving one is sound
// because no clone of those `Rc`s stays behind on the loader thread: it builds the whole engine
// there, never runs the model (neither `load` processes a frame), tract's thread-locals hold
// only scratch memory of their own (per-session scratch, created when a session runs), and the
// thread ends right after the send. The channel orders
// the build before any use. Re-check on a tract upgrade: a thread-local caching a model `Rc`
// (or a warm-up run in `load`) would break this.
unsafe impl Send for HandOver {}

/// Loads a heavy engine, logging the outcome (also on the loader thread; audiodg has no logger).
fn load_logged<E>(name: &str, load: fn() -> Result<E, String>) -> Result<Box<E>, String> {
  let t0 = Instant::now();
  match load() {
    Ok(engine) => {
      log::info!("[Denoise] {name} model loaded in {:?}", t0.elapsed());
      Ok(Box::new(engine))
    }
    Err(e) => {
      log::error!("[Denoise] {name} failed to load, staying on RNNoise: {e}");
      Err(e)
    }
  }
}

pub(super) fn load_deepfilter() -> Loaded {
  Loaded::DeepFilter(load_logged("DeepFilterNet 3", DeepFilterEngine::load))
}

pub(super) fn load_dpdfnet() -> Loaded {
  Loaded::Dpdf(load_logged("DPDFNet", DpdfEngine::load))
}

/// A load of `model` that could not run (no thread, or the loader died).
pub(super) fn load_failed(model: DenoiseModel, why: String) -> Loaded {
  log::error!("[Denoise] {model:?} failed to load, staying on RNNoise: {why}");
  match model {
    DenoiseModel::DpdfNet => Loaded::Dpdf(Err(why)),
    _ => Loaded::DeepFilter(Err(why)),
  }
}

pub(super) fn put<E>(slot: &mut Option<Box<E>>, error: &mut Option<String>, loaded: Result<Box<E>, String>) {
  match loaded {
    Ok(engine) => *slot = Some(engine),
    Err(e) => *error = Some(e),
  }
}
