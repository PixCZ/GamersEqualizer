//! `process_frame` and `voice_probability` must not touch the heap (it runs on the real-time audio thread).

#![allow(clippy::unwrap_used, reason = "test")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use rnnoise2::{DenoiseState, Model, FRAME_SIZE};

/// Counts allocations made on threads that opted in (the test harness's own threads don't count).
struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
  static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY-free wrapper: delegates to `System`; `GlobalAlloc` itself is an unsafe trait.
#[allow(unsafe_code, reason = "a counting global allocator needs the unsafe GlobalAlloc trait")]
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if COUNTING.with(Cell::get) {
      ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: same contract as the caller's.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: same contract as the caller's.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    if COUNTING.with(Cell::get) {
      ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: same contract as the caller's.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn process_frame_does_not_allocate() {
  for model in [Model::Full, Model::Little] {
    let mut state = DenoiseState::with_model(model).unwrap();
    let mut out = [0.0f32; FRAME_SIZE];
    // Speech-like level so the network path runs, plus silent frames for the other path.
    let input: Vec<[f32; FRAME_SIZE]> = (0..100)
      .map(|f| {
        let mut frame = [0.0f32; FRAME_SIZE];
        if f % 10 != 0 {
          for (i, s) in frame.iter_mut().enumerate() {
            let t = (f * FRAME_SIZE + i) as f32;
            *s = 3000.0 * (t * 0.031).sin() + 800.0 * (t * 0.37).sin();
          }
        }
        frame
      })
      .collect();
    COUNTING.with(|c| c.set(true));
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut voiced = false;
    for frame in &input {
      voiced |= state.process_frame(&mut out, frame) > 0.0;
    }
    state.reset();
    for frame in &input {
      voiced |= state.voice_probability(frame) > 0.0;
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    COUNTING.with(|c| c.set(false));
    assert_eq!(after, before, "{model:?}: process_frame/reset/voice_probability allocated");
    assert!(voiced, "{model:?}: the network path was never taken (VAD stayed 0)");
  }
}
