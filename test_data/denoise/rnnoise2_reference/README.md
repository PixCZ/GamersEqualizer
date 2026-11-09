# RNNoise reference outputs (`rnnoise2`)

Output of the original C RNNoise, used by the `rnnoise2` crate's reference test
(`crates/vendor/rnnoise2`). Test-only, never shipped. Total 1.94 MB (9 WAVs, 48 kHz mono 16-bit).

| File                    | Content                                                                     |
| ----------------------- | --------------------------------------------------------------------------- |
| `in_speech.wav`         | `speech_clean.wav` from 1.00 s, 2.25 s (225 frames of 480)                  |
| `in_keys.wav`           | the same speech + keyboard (`1-137-A-32.wav` from 0.80 s) at -10 dB SNR     |
| `in_clicks.wav`         | 0.25 s digital silence, then mouse clicks (`1-118206-A-31.wav` from 0.30 s) |
| `out_<clip>_full.wav`   | upstream output with the Full model, 224 frames                             |
| `out_<clip>_little.wav` | upstream output with the Little model, 224 frames                           |

Inputs: `python3 -I crates/vendor/rnnoise2/tools/make_reference_inputs.py` (stdlib only;
the 44.1 kHz ESC-50 clips are resampled to 48 kHz by linear interpolation; speech and mix share
one gain so the mix peaks at -1 dBFS). Licenses of the source clips: see `../README.md`.

## How the outputs were made

- Code: [xiph/rnnoise](https://github.com/xiph/rnnoise) `main` at
  `70f1d256acd4b34a572f999a05c87bf00b67730d` (2025-02-22), the commit whose `model_version` is
  the model below. Not tag `v0.2`: that tag pins an older model (`0b50c45`) and has no skip
  connections, see `crates/vendor/rnnoise2/README.md`.
- Model: `rnnoise_data-0a8755f8e2d834eff6a54714ecc7d75f9932e845df35f8b59bc52a7cfe6e8b37.tar.gz`,
  loaded as the blobs `crates/vendor/rnnoise2/models/rnnoise_{full,little}.bin` (byte-identical
  to upstream `dump_weights_blob`). A build with the compiled-in Full arrays gave identical output.
- Binary: `examples/rnnoise_demo.c` built with Apple clang 21 on arm64, outside the repo:
  `clang -O2 -DRNNOISE_BUILD -DDISABLE_DEBUG_FLOAT -DUSE_WEIGHTS_FILE -DDISABLE_NEON -ffp-contract=off`
  over `denoise.c rnn.c pitch.c kiss_fft.c celt_lpc.c nnet.c nnet_default.c parse_lpcnet_weights.c
  rnnoise_tables.c rnnoise_data.c rnnoise_demo.c -lm`. `DISABLE_NEON` selects the generic C
  kernels (`vec.h`) and `-ffp-contract=off` forbids FMA fusion, i.e. plain IEEE f32 like the Rust
  port. With `-DUSE_WEIGHTS_FILE` the demo reads `weights_blob.bin` from its working directory,
  so one binary covered both models.
- For scale: the default arm64 build (NEON kernels, FMA on) differs from these files by up to
  90 LSB (Little, speech) and 1.7–4.8 LSB RMS on speech; on clicks by at most 2 LSB.

## Demo I/O and framing (the Rust test must mirror this)

- Input and output are headerless native-endian `int16` PCM; the WAVs here hold the same samples.
  Each sample goes in as `float` in the int16 scale; output is cast back with a C `(short)` cast
  (truncation toward zero, no clamp; no output here comes near the int16 limits).
- Frames of 480 samples are read until a short read; a trailing partial frame is dropped
  (inputs here are exact multiples).
- **The first output frame is not written.** Output file sample `n` is `process_frame` output
  sample `n + 480`.
- `process_frame` delays by 960 samples (2 frames: 1 frame overlap-add + 1 frame `delayed_X`),
  so after dropping one frame the output file lags the input by 480 samples (measured by
  cross-correlation on `in_speech.wav`).
- One `DenoiseState` per file, fresh state, no reset in between.
