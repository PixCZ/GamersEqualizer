# silero_vad

Pure-Rust port of [Silero VAD](https://github.com/snakers4/silero-vad) v6 (tag v6.2.3, the 16 kHz
network) for the mic chain: a voice detector that gives one speech probability per 32 ms. No ONNX
runtime, no FFI, no dependencies.

Status: the port matches onnxruntime running the upstream model (see "Verification"). **Not wired
into the mic chain yet.** Why it exists: DEVLOG milestones 38 (what it hears, offline) and 39.

Why a port: tract 0.23.10, the runtime inside the APO, does not load the published ONNX files
(`silero_vad_16k_op15.onnx`: an `If` whose branches differ in shape; `silero_vad_op18_ifless.onnx`:
an `If` remains). The network is small enough to write out by hand.

## Interface

```rust
pub const SAMPLE_RATE: usize = 16_000;
pub const CHUNK: usize = 512;        // samples per call at 16 kHz (32 ms)
pub const CHUNK_48K: usize = 1536;   // the same chunk at 48 kHz
pub enum WeightsError { .. }         // corrupt blob; Display + Error
pub struct SileroVad { .. }          // 16 kHz
impl SileroVad {
  pub fn new() -> Result<Box<Self>, WeightsError>;
  pub fn process_chunk(&mut self, chunk: &[f32]) -> f32;   // -1..1 samples in, probability 0..1 out
  pub fn reset(&mut self);
}
pub struct Vad48k { .. }             // 48 kHz blocks of any size
impl Vad48k {
  pub fn new() -> Result<Self, WeightsError>;
  pub fn process(&mut self, samples: &[f32], on_prob: impl FnMut(f32));  // once per finished chunk
  pub fn reset(&mut self);           // new stream
  pub fn flush(&mut self);           // drop buffered audio, keep the network state
}
```

Construction returns `Result` (a corrupt blob is an `Err`, never a panic). `process_chunk` and
`process` never allocate and never panic: a short chunk is zero-padded, NaN and infinite samples
count as 0. The blob is parsed once per process (1.2 MB of f32) and shared by all instances;
an instance holds about 13 KB of state (`Vad48k`).

`Vad48k` low-passes (63-tap windowed sinc, cutoff 7.2 kHz, 0.65 ms delay) and keeps every third
sample. The probability of a chunk arrives when its last sample does, so it describes audio up
to 32 ms old.

## The network

| Stage    | Shape                                                                                   |
| -------- | --------------------------------------------------------------------------------------- |
| Input    | 64 samples of the previous chunk + 512 new + 64 reflected = 640                         |
| STFT     | 258 x 256 filter bank (129 real, 129 imaginary rows), hop 128: 4 frames of 129 magnitudes |
| Encoder  | conv 129 -> 128, 128 -> 64 (stride 2), 64 -> 64 (stride 2), 64 -> 128; kernel 3, ReLU     |
| Decoder  | LSTM cell 128, ReLU, 128 -> 1, sigmoid                                                   |

About 0.7 million multiply-adds per chunk. State: the LSTM's hidden and cell vectors and the 64
context samples.

## Origin

- Code: written from the graph of `src/silero_vad/data/silero_vad_16k_op15.onnx` at tag v6.2.3
  (sha256 `7ed98ddbad84ccac4cd0aeb3099049280713df825c610a8ed34543318f1b2c49`). MIT, see
  `LICENSE-silero-vad`.
- Weights: the initializers of that file, as `models/silero_vad_v6.bin` (sha256 next to it), made
  by `tools/convert_weights.py`. Blob: magic `SVAD6\0\0\0`, then per tensor a u32 count and
  little-endian f32.
- `silero_vad_16k.safetensors` in the same upstream folder is **another model**: every layer but
  the STFT differs and its output is off by up to 0.46. Not used.
- The 8 kHz branch of the full model is not ported.

## Verification

`cargo test --manifest-path crates/vendor/silero_vad/Cargo.toml --release -- --nocapture`

| Test                                 | Result                                                                    |
| ------------------------------------ | ------------------------------------------------------------------------- |
| `matches_onnxruntime`                | 312 chunks (10 s), largest difference 1.5e-6 (limit 1e-4)                 |
| `hears_speech_and_not_keys_at_48k`   | clean speech: 198 of 199 loud chunks over 0.5; ESC-50 keys alone: highest 0.08 |
| `reset_starts_the_same_stream_again` | identical output after `reset`                                            |
| `bad_input_never_sticks`             | NaN, infinity, short and empty chunks behave as silence                   |

Cost with the 48 kHz front end: 0.16 % of real time on one thread (the developer's PC, release).
Reference files: `test_data/denoise/silero_reference/` (see its README).
