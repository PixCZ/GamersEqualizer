# rnnoise2

Pure-Rust port of RNNoise as of its 2024–2025 rewrite (the "0.2" network: conv1d + 3 GRU + skip
connections, 32 bands) for the mic chain (`gamersequalizer-mic-vst`, `ai_denoiser.rs`). Forked
from nnnoiseless 0.5.2, which implements the 2017 network. No C, no FFI. The spec
(`docs/rnnoise-0.2-port.md`) was removed from the repo with the working reports; see git history.

Status: the port (spec chapters 4–7) is done and matches the upstream C output within the spec 6.2
thresholds for both models (see "Verification"). Wired into the mic chain as the raw-input voice
detector only (`ai_denoiser.rs` `in_net`, `Full`, `voice_probability`; DEVLOG milestone 35; Windows
verification pending). The spec chapter 7 gate for the *filter* is **not met**: pause suppression is
far better, but speech loss under keys is worse (see "Quality vs nnnoiseless"), so the filter, the
pause stand-in, `out_net` and `ClickRescue` stay on nnnoiseless. As a detector, `Full` hears far more
speech at the same false-alarm rate (see "Voice detector operating points").

## Interface

```rust
pub const FRAME_SIZE: usize = 480;      // samples per frame, 48 kHz
pub const LATENCY_FRAMES: usize = 2;    // output lags input by 960 samples (nnnoiseless: 1 frame)
pub enum Model { Full, Little }         // both embedded via include_bytes!
pub enum WeightsError { .. }            // corrupt blob; Display + Error
pub struct DenoiseState { .. }
impl DenoiseState {
  pub const FRAME_SIZE: usize = 480;
  pub fn new() -> Result<Box<Self>, WeightsError>;            // Model::Full
  pub fn with_model(model: Model) -> Result<Box<Self>, WeightsError>;
  pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32; // i16-range f32, returns VAD
  pub fn voice_probability(&mut self, input: &[f32]) -> f32;  // VAD only: analysis + network, no output
  pub fn reset(&mut self);
}
```

One deliberate change from the spec's interface: construction returns `Result` (a corrupt blob is
an `Err`, never a panic). `process_frame` never allocates (`tests/no_alloc.rs`), never panics on
short slices (input zero-padded, only the first 480 output samples written) and returns 0 VAD on
silent frames. `voice_probability` is the detector-only variant: same input handling, same VAD
sequence bit for bit (`voice_probability_matches_process_frame`), but no pitch filter, gains or
synthesis; it skips that output-side state, so use an instance for one or the other (or `reset()`
when switching). Neither allocates. The blob is parsed once per instance: f32 arrays are copied (about 0.3 MB), int8
weights are borrowed from the embedded blob.

## Modules

| File             | Upstream                                            | Content                                                                                 |
| ---------------- | --------------------------------------------------- | --------------------------------------------------------------------------------------- |
| `src/lib.rs`     | `rnnoise.h`                                         | public interface, embedded blobs                                                        |
| `src/weights.rs` | `parse_lpcnet_weights.c`, `init_rnnoise`            | record parser and checks, `WeightsError`                                                |
| `src/nnet.rs`    | `nnet.c`, `nnet_arch.h`, `vec.h` (generic), `rnn.c` | f32 dense, int8 layers (quantize, scale, diag), conv1d, GRU, activations, `compute_rnn` |
| `src/kernel.rs`  | `vec.h` `cgemv8x4`, `sparse_cgemv8x4`               | int8 8x4-block mat-vec: scalar reference, AVX2 (x86_64), `dotprod` (aarch64); `unsafe`  |
| `src/bands.rs`   | `denoise.c`, `rnnoise_tables.c`                     | `eband20ms` energies/correlations, gain interpolation, DCT, window                      |
| `src/pitch.rs`   | `pitch.c`, `celt_lpc.c`                             | pitch downsample/search/`remove_doubling` (from nnnoiseless, C summation order)         |
| `src/denoise.rs` | `denoise.c`                                         | per-frame pipeline, HP biquad, FFT (rustfft), pitch filter, synthesis                   |
| `tests/`         |                                                     | `reference.rs` (6.2, 6.4 latency), `no_alloc.rs`, `report.rs` (6.3, 6.4 bench, ignored) |

Tests: `cargo test --manifest-path crates/vendor/rnnoise2/Cargo.toml` (about 15 s unoptimized;
add `--release` for under a second). Reports: `... --release --test report -- --ignored --nocapture --test-threads=1`.

## Origin

| What             | Value                                                                                                                        |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Upstream         | [xiph/rnnoise](https://github.com/xiph/rnnoise) `main` @ `70f1d256acd4b34a572f999a05c87bf00b67730d` (2025-02-22)             |
| Model tarball    | `https://media.xiph.org/rnnoise/models/rnnoise_data-0a8755f8e2d834eff6a54714ecc7d75f9932e845df35f8b59bc52a7cfe6e8b37.tar.gz` |
| Model SHA-256    | `0a8755f8e2d834eff6a54714ecc7d75f9932e845df35f8b59bc52a7cfe6e8b37` (= upstream `model_version` at that commit)               |
| Compared against | tag `v0.2` @ `904a876dce1f9ab8860c0a5000ed151f9f6eef58`, tag `v0.1.1` @ `6cbfd53eb348a8d394e0757b4025c6ded34eb2b6`           |
| Fork base        | nnnoiseless 0.5.2 (crates.io, vcs `924a2dd143ccad7bce9e5bda061b60ca32911a67`)                                                |
| License          | BSD-3-Clause, both: `LICENSE-rnnoise` (upstream `COPYING`), `LICENSE-nnnoiseless` (its `COPYING`), unchanged                 |

### Why `main` and not tag `v0.2`

The model hash above is not the one tag `v0.2` pins: `v0.2` has `model_version` `0b50c45`, a
model without skip connections (`dense_out` reads only GRU3). `main` retrained the model
(augmentation, more noise data) and changed two runtime things since `v0.2`:

- `rnn.c`: skip connections. `dense_out` and `vad_dense` read `cat = [conv2 | gru1 | gru2 | gru3]`
  (1536 values) instead of `gru3` (384). The `0a8755f8` weights only fit this graph.
- `denoise.c`: the gain-decay floor compensates for energy change (see Port notes, step 7).

Everything else between `v0.2` and `main` is `(void)` prototypes, include fixes and training code.
There is no newer release tag; this crate tracks `main` @ `70f1d25`.

## Network dimensions

`rnnoise_data.h` is identical for both models. Little has the same shapes, only sparser GRU matrices.

| Layer              | In → out    | Kernel   | Weights (Full)                  | Weights (Little)               | Activation |
| ------------------ | ----------- | -------- | ------------------------------- | ------------------------------ | ---------- |
| `conv1`            | 65 → 128    | 3 frames | f32 dense 195×128               | same                           | tanh       |
| `conv2`            | 128 → 384   | 3 frames | int8 dense 384×384              | same                           | tanh       |
| `gru1_input`       | 384 → 1152  |          | int8 sparse, 13824 blocks (all) | int8 sparse, 4608 blocks (1/3) |            |
| `gru1_recurrent`   | 384 → 1152  |          | int8 sparse 13824 + `diag`      | int8 sparse 4608 + `diag`      |            |
| `gru2_*`, `gru3_*` | as `gru1_*` |          | as `gru1_*`                     | as `gru1_*`                    |            |
| `dense_out`        | 1536 → 32   |          | f32 dense                       | same                           | sigmoid    |
| `vad_dense`        | 1536 → 1    |          | f32 dense                       | same                           | sigmoid    |

Header constants: `CONV1_IN_SIZE 65`, `CONV1_OUT_SIZE 128`, `CONV1_STATE_SIZE 130`, `CONV1_DELAY 1`,
`CONV2_IN_SIZE 128`, `CONV2_OUT_SIZE 384`, `CONV2_STATE_SIZE 256`, `CONV2_DELAY 1`,
`GRU{1,2,3}_OUT_SIZE = GRU{1,2,3}_STATE_SIZE = 384`, `DENSE_OUT_OUT_SIZE 32`, `VAD_DENSE_OUT_SIZE 1`;
`NB_BANDS 32`, `NB_FEATURES 65` (`denoise.h`). Kernel width = `nb_inputs / input_size` (195/65, 384/128).
A "block" is 8 outputs × 4 inputs; the Full GRU matrices list every block (dense in sparse format).

## Weight blobs

`models/rnnoise_{full,little}.bin`, made by `tools/convert_weights.py` from the tarball's
`src/rnnoise_data{,_little}.c`. Byte-identical to upstream `dump_weights_blob`
(`src/write_weights.c`, built with `DISABLE_DEBUG_FLOAT` like the default `configure`).

| Blob                 | Bytes     | Records | SHA-256                                                            |
| -------------------- | --------- | ------- | ------------------------------------------------------------------ |
| `rnnoise_full.bin`   | 3 544 320 | 43      | `1ad07b428be34c74d9678d05b3c387b73ccf0e6203a1a5f9ce58689b30b303c8` |
| `rnnoise_little.bin` | 1 553 664 | 43      | `69fbd33c10d90e294673427f71fc4a592398d847efb2db19c99484f88b9343e4` |

```
python3 -I crates/vendor/rnnoise2/tools/convert_weights.py <tarball>/src/rnnoise_data.c        crates/vendor/rnnoise2/models/rnnoise_full.bin
python3 -I crates/vendor/rnnoise2/tools/convert_weights.py <tarball>/src/rnnoise_data_little.c crates/vendor/rnnoise2/models/rnnoise_little.bin
```

Record format (`nnet.h:41-62`, `write_weights.c:46-69`, `parse_lpcnet_weights.c:37-52`):

| Offset | Size   | Field                                                     |
| ------ | ------ | --------------------------------------------------------- |
| 0      | 4      | `head` = `"DNNw"` (not checked by the parser)             |
| 4      | i32    | `version` = 0 (not checked)                               |
| 8      | i32    | `type`: 0 float, 1 int, 2 qweight (unused), 3 int8        |
| 12     | i32    | `size`: payload bytes                                     |
| 16     | i32    | `block_size`: `size` rounded up to 64                     |
| 20     | 44     | `name`, NUL padded, last byte must be 0                   |
| 64     | `size` | payload (f32 / i32 / i8, machine endian = little)         |
| …      | pad    | zeros up to `block_size`; next record at `+64+block_size` |

Parser checks (`parse_record`): `len >= 64`, `block_size >= size`, `block_size <= len-64`,
`name[43] == 0`, `size >= 0`, and the whole parse fails on a record with `size == 0`. Lookup is a
linear search by name; `linear_init` (`parse_lpcnet_weights.c:123-176`) requires each named array
to exist with exactly the expected byte size: `bias`, `subias`, `diag`, `scale` = `nb_outputs`×4;
dense int8 = `nb_inputs`×`nb_outputs`; sparse int8 = 32 × total blocks from `*_idx`;
`*_weights_float` is optional (absent in these blobs). `find_idx_check` (`:98-121`): per group of 8
outputs, `count` then `count` input offsets, each a multiple of 4 with `pos+3 < nb_inputs`;
the groups must cover exactly `nb_outputs`. Payload offsets are 64-byte aligned.

Records, in order (both blobs; `int8`/`idx` sizes Full / Little):

| Layer (`init_rnnoise`)       | Arrays                                                                                                     |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `conv1` (195, 128)           | `conv1_weights_float` f32 24960, `conv1_bias` f32 128                                                      |
| `conv2` (384, 384)           | `conv2_weights_int8` i8 147456, `conv2_subias`, `conv2_scale`, `conv2_bias` f32 384                        |
| `gruN_input` (384, 1152)     | `_weights_int8` i8 442368 / 147456, `_weights_idx` i32 13968 / 4752, `_subias`, `_scale`, `_bias` f32 1152 |
| `gruN_recurrent` (384, 1152) | `_weights_diag` f32 1152, `_weights_int8`, `_weights_idx`, `_subias`, `_scale`, `_bias` (as input)         |
| `dense_out` (1536, 32)       | `dense_out_weights_float` f32 49152, `dense_out_bias` f32 32                                               |
| `vad_dense` (1536, 1)        | `vad_dense_weights_float` f32 1536, `vad_dense_bias` f32 1                                                 |

`subias` is only used by the x86 AVX path (`USE_SU_BIAS`); the generic path uses `bias`. `scale`
is shipped per output in the C arrays, nothing to compute.

## Feature vector (65 values, `denoise.c:347-398`)

| Index  | Value                                                                                                         |
| ------ | ------------------------------------------------------------------------------------------------------------- |
| 0..32  | `dct(Ly)`, then `[0] -= 12`, `[1] -= 4`; `Ly[i] = log10(1e-2 + Ex[i])` floored by `max(logMax-7, follow-1.5)` |
| 32..64 | `dct(Exp)` with `Exp[i] = corr(X,P)[i] / sqrt(.001 + Ex[i]*Ep[i])`, no offsets                                |
| 64     | `.01 * (pitch_index - 300)`                                                                                   |

No deltas, no cepstral memory, no spectral variability (all removed since v0.1.1).

## Layer graph (`rnn.c:44-60`, call order)

```
tmp[128]  = conv1d(conv1, state conv1_state[130], features[65], tanh)
cat[0..384] = conv1d(conv2, state conv2_state[256], tmp[128], tanh)
gru1_state = gru(gru1_input, gru1_recurrent, gru1_state, cat[0..384])
gru2_state = gru(gru2_*, gru2_state, gru1_state)
gru3_state = gru(gru3_*, gru3_state, gru2_state)
cat[384..768] = gru1_state; cat[768..1152] = gru2_state; cat[1152..1536] = gru3_state
gains[32] = sigmoid(dense_out · cat + b);  vad = sigmoid(vad_dense · cat + b)
```

## Port notes

Everything the Rust port must reproduce to match the C reference (`test_data/denoise/rnnoise2_reference/`).
Line numbers refer to `main` @ `70f1d25`.

### Network (`nnet.c`, `nnet_arch.h`, `vec.h` generic path)

1. **conv1d** (`nnet.c:113-123`): `tmp = [mem | input]` (`nb_inputs` = 3 × `input_size`, oldest
   frame first), `out = act(linear(tmp))`, then `mem = tmp[input_size..]`. State = the previous two
   input frames, oldest first. No padding, no delay compensation (`CONV*_DELAY` is informational).
2. **linear** (`nnet_arch.h:130-162`): float weights → `sgemv`; int8 → `cgemv8x4` / `sparse_cgemv8x4`
   × `scale[i]`; then `+ bias[i]` (not `subias`); then, if `diag`, for `i < M`:
   `out[i] += diag[i]*in[i]; out[i+M] += diag[i+M]*in[i]; out[i+2M] += diag[i+2M]*in[i]`.
3. **f32 dense** (`vec.h:49-121`): column-major, `out[i] = Σ_j w[j*N + i] * x[j]`, summed in `j`
   order from 0.0 (all three variants, `sgemv16x1`/`8x1`/scalar, keep this order per output).
4. **int8** (`vec.h:248-311`, the non-`USE_SU_BIAS` branch): the input is quantized first,
   `xq[j] = (i8)(int)floor(.5 + (double)(127.f * x[j]))` (127·x in f32, rounding in double; inputs are
   tanh / GRU state, so within ±127). Weights are blocks of 8 outputs × 4 inputs, stored
   `w[k*4 + c]` for output `i+k`, input `j+c`. Dense: blocks ordered by output group, then input
   group `j = 0, 4, 8, …`. Sparse: per output group `count`, then `count` offsets `j`, one block each.
   Each block adds `y[k] += (w[k*4]*x0 + w[k*4+1]*x1 + w[k*4+2]*x2 + w[k*4+3]*x3)`, the 4-term sum
   exact in integers, the accumulation in f32 starting at 0. Every partial sum is an integer below
   384 · 128 · 128 < 2^24 (the loader rejects more blocks per output group than input groups), so
   that f32 sum is exact and the port accumulates in i32 instead (`kernel.rs`, any order, SIMD),
   bit-identical. Finally `out[i] *= scale[i]`. Effective weight = `q * scale * 127`.
   `WEIGHTS_SCALE (1/256)` in `rnn.h` is dead.
5. **GRU** (`nnet.c:65-94`), gate order z, r, h in the 1152 outputs:
   `zrh = linear_in(x)`, `rec = linear_rec(state)` (incl. bias and diag);
   `z,r = sigmoid(zrh[0..2N] + rec[0..2N])`; `h = tanh(zrh[2N..] + r * rec[2N..])`;
   `state = z*state + (1-z)*h`.
6. **Activations** (`vec.h:337-381`), no lookup tables:
   ```
   tanh_approx(x): X2 = x*x
     num = (0.60863042*X2 + 96.39235687)*X2 + 952.52801514
     den = (11.88600922*X2 + 413.36801147)*X2 + 952.72399902
     return clamp(num*x/den, -1, 1)          // max(-1, min(1, .)), f32, (num*x)/den
   sigmoid_approx(x) = 0.5 + 0.5*tanh_approx(0.5*x)
   ```
   All f32, no FMA (`fmadd(a,b,c)` is `a*b+c`; the reference was built with `-ffp-contract=off`).
   NEON/AVX builds use other approximations (`exp`-based sigmoid in NEON); not ported.

### Signal path (`denoise.c`, `pitch.c`, `celt_lpc.c`, `kiss_fft.c`)

1. Frame 480, window 960, `FREQ_SIZE` 481, 48 kHz. Window `rnn_half_window` equals
   `(float)sin(.5π·sin²(.5π(i+.5)/480))` computed in double (checked: all 480 values), same as nnnoiseless.
2. FFT (`forward_transform`): forward DFT scaled by 1/960 (`rnn_kfft.scale` = `0.0010416667f`);
   inverse = forward FFT of the Hermitian-extended spectrum, read in reverse, × 960, i.e. an
   unscaled IDFT. Kiss applies the scale to the FFT *input* (`rnn_fft_c`), also inside the inverse,
   so the port does the same (input × `0.0010416667f`, complex 960-point rustfft, output × 960
   on the inverse) rather than scaling the output. Kiss factors 5·3·4·4·4; rustfft sums in
   another order (unavoidable small difference).
3. High-pass `rnn_biquad` (`:409-419`) before analysis: `b = {-2, 1}`, `a = {-1.99599f, 0.99600f}`,
   `yi = x + mem0` (f32), `mem0 = (f32)(mem1 + (b0*(double)xi - a0*(double)yi))`,
   `mem1 = (f32)(b1*(double)xi - a1*(double)yi)`.
4. Bands `eband20ms` (34 edges, `:63-65`):
   `0 2 4 6 8 10 12 15 18 21 24 28 32 36 41 47 53 60 68 77 87 98 110 124 140 157 176 198 223 251 282 317 356 400`.
   `compute_band_energy`/`corr` (`:90-138`): triangular split over 33 intervals into `sum[34]`
   (`frac = (float)j/band_size`), then `sum[1] = (sum[0]+sum[1])*2/3`,
   `sum[32] = (sum[32]+sum[33])*2/3`, `bandE[i] = sum[i+1]`. Bins ≥ 400 are ignored.
5. `interp_band_gain` (`:140-154`): bins `[e[i], e[i+1])` for i = 1..31 interpolate
   `bandE[i-1] → bandE[i]`; bins 0..1 = `bandE[0]`; bins 356..399 = `bandE[31]`; **bins 400..480 are
   never written** and stay at the caller's initial value, which is 0 in all three callers
   (`gf`, `rf`, `normf`), so bins ≥ 400 end up zeroed whenever gains apply.
   (`memset(g, 0, FREQ_SIZE)` clears bytes, not floats; harmless.)
6. DCT (`:160-170`): 32×32 table `cos((i+.5)·j·π/32)`, `j == 0` × `sqrt(.5)`, but output scaled by
   **`sqrt(2./22)`** (left over from 22 bands; nnnoiseless uses `sqrt(2/NB_BANDS)`, which would be wrong here).
   `out[i] = (f32)((double)Σ_j in[j]*table[j*32+i] * sqrt(2./22))`.
7. Per frame (`rnnoise_process_frame`, `:457-504`):
   1. `x = biquad(in)`; features from `x` (analysis, pitch, `Ex`, `Ep`, `Exp`, `X`, `P`).
   2. Silence: `E = Σ Ex[i]` (f32) `< 0.04` → features cleared, **RNN not run** (conv/GRU state not
      advanced), VAD = 0, `lastg` unchanged, and `delayed_X` is synthesized **without** gain or pitch filter.
   3. Otherwise: `g, vad = compute_rnn(features)`; `rnn_pitch_filter(delayed_X, delayed_P,
      delayed_Ex, delayed_Ep, delayed_Exp, g)`; per band `g = max(g, 0.6*lastg)`,
      `lastg = min(1, g*(delayed_Ex+1e-3)/(Ex+1e-3))` (double arithmetic, new on `main`);
      `delayed_X *= interp_band_gain(g)`.
   4. Synthesize `delayed_X` (IFFT, window, overlap-add with `synthesis_mem`).
   5. `delayed_{X,P,Ex,Ep,Exp} = current`. Gains from frame t are applied to frame t-1.
   Total delay of `process_frame`: 960 samples = **2 frames** (nnnoiseless: 1).
8. `rnn_pitch_filter` (`:421-455`): per band `r = 1` if `Exp > g`, else
   `Exp²(1-g²)/(.001 + g²(1-Exp²))`, `r = sqrt(clamp(r, 0, 1)) * sqrt(Ex/(1e-8+Ep))`; `X += interp(r)·P`;
   then renormalize `X *= interp(sqrt(Ex/(1e-8+newE)))`. Uses the gains before the 0.6 floor.
9. `Exp[i] = Exp[i] / sqrt(.001 + Ex[i]*Ep[i])`: product in f32, the rest in double.
10. Pitch: `rnn_pitch_downsample`, `rnn_pitch_search`, `rnn_remove_doubling`, `celt_lpc` are
    algorithmically unchanged since v0.1.1 (only VLAs became fixed arrays), so nnnoiseless
    `pitch.rs` carries over, **but not its summation order** (correction found in phase 2):
    nnnoiseless's `inner_prod` sums in four partial sums (C `celt_inner_prod`/`dual_inner_prod`:
    one sequential sum), its `yy` update is `yy + (a - b)` (C: `(yy + a) - b`), it lacks the
    float-only `xcorr16 *= 1e-12f` in `find_best_pitch`, and its pitch gain uses an f32 `sqrt`
    (C: `xy / sqrt(1 + xx*yy)` in double). The unrolled C `xcorr_kernel` is, per lag, one
    sequential sum, so a plain loop reproduces it. nnnoiseless's biquad also keeps `y` in f64 for
    the memory update; C rounds `yi` to f32 first (step 3). `PITCH_MIN_PERIOD 60`, `PITCH_MAX_PERIOD 768`, `PITCH_FRAME_SIZE 960`,
    `PITCH_BUF_SIZE 1728`; the pitch buffer gets the high-passed `x`.
11. C mixes f32 and double (`.001`, `1e-2`, `sqrt`, `log10`, `.01*`); mirror it with `f64`
    intermediates cast back to `f32` at the same points, as nnnoiseless does.

### Reference tolerance

The default arm64 upstream build (NEON, FMA) already differs from the generic-C reference by up to
90 LSB and 1.7–4.8 LSB RMS on speech, because int8 input quantization and pitch decisions amplify
last-bit differences through the recurrent state. The port's remaining difference (rustfft vs kiss
summation order, libm) is below that and within the spec thresholds as they are, see "Verification".

## Verification

`reference_matches_upstream` (spec 6.2) runs `test_data/denoise/rnnoise2_reference/in_*.wav`
through a fresh state, aligned and truncated like `rnnoise_demo` (reference sample n = output
sample n + 480, `(short)` cast). Thresholds: max 1e-3 FS = 32.8 LSB, RMS 1e-4 FS = 3.28 LSB.
Measured (identical in debug and release on macOS arm64):

| Clip   | Model  | Max error [LSB] | RMS error [LSB] |
| ------ | ------ | --------------- | --------------- |
| speech | Full   | 1               | 0.005           |
| speech | Little | 1               | 0.036           |
| keys   | Full   | 11              | 0.589           |
| keys   | Little | 19              | 0.921           |
| clicks | Full   | 1               | 0.038           |
| clicks | Little | 0               | 0.000           |

`latency_matches_constant` (spec 6.4): a tiny impulse on the silence path and loud white noise
through the network both come out exactly `LATENCY_FRAMES * FRAME_SIZE` = 960 samples later, for
both models. So `LATENCY_FRAMES = 2` (nnnoiseless: 1).

## Quality vs nnnoiseless

`rnnoise2_report` (spec 6.3), metrics copied from `ai_denoiser.rs` (`measure`, the loss half of
`run_clicks`), shared fixtures: `speech_clean.wav` at -20 dBFS plus the six ESC-50 keyboard/mouse
clips, each model aligned by its own latency, plain RNNoise output (no gate, no strength mix).
Loud/soft lost: share of speech frames >= clean RMS / >= clean RMS - 15 dB losing more than 15 dB.
Mean loss / lost > 6 dB: speech frames with clicks no more than 10 dB below the speech. Soft
syllables: speech frames below -35 dBFS (down to the pause level, clean RMS - 40 dB) losing more
than 10 dB. VAD on speech: share of speech frames (>= clean RMS - 15 dB) with probability >= 0.5, the
probability paired with the frame the model's gains target (rnnoise2: one frame later than the input).

| Mix        | Model           | Pause supp. dB | Far pause supp. dB | Loud lost % | Soft lost % | Mean loss dB | Lost >6 dB % | Soft syll. lost % | VAD on speech % |
| ---------- | --------------- | -------------- | ------------------ | ----------- | ----------- | ------------ | ------------ | ----------------- | --------------- |
| 0 dB SNR   | nnnoiseless 0.5 | 18.3           | 19.9               | 0.0         | 0.0         | 1.36         | 6.1          | 8.6               | 81.7            |
| 0 dB SNR   | rnnoise2 Full   | 41.4           | 54.6               | 0.0         | 0.9         | 1.75         | 6.5          | 30.2              | 99.0            |
| 0 dB SNR   | rnnoise2 Little | 35.8           | 68.2               | 0.0         | 3.1         | 2.33         | 6.5          | 32.7              | 97.4            |
| -10 dB SNR | nnnoiseless 0.5 | 18.0           | 19.7               | 0.0         | 1.9         | 2.23         | 11.9         | 9.3               | 75.6            |
| -10 dB SNR | rnnoise2 Full   | 41.9           | 49.4               | 0.7         | 5.0         | 3.90         | 10.9         | 34.6              | 96.5            |
| -10 dB SNR | rnnoise2 Little | 42.8           | 57.2               | 0.3         | 6.9         | 4.56         | 12.2         | 37.0              | 95.9            |

Keys only at -20 dBFS (no speech):

| Model           | Suppression dB | VAD >= 0.5 % |
| --------------- | -------------- | ------------ |
| nnnoiseless 0.5 | 18.6           | 0.4          |
| rnnoise2 Full   | 36.5           | 1.3          |
| rnnoise2 Little | 43.7           | 18.3         |

Reading: 0.2 suppresses keys in pauses about twice as hard in dB (Full 36–41 dB vs 18 dB) and its
VAD fires on far more speech frames (96–99 % vs 76–82 %), but it cuts quiet syllable onsets/tails
much more (30–37 % vs 9 %) and loses slightly more speech under keys. On keys alone Full's VAD
stays low (1.3 %), Little's does not (18 %). Spec chapter 7 asks for VAD on keys <= the old one:
neither model strictly meets it (Full 1.3 % vs 0.4 %).

### Voice detector operating points

`vad_sweep`: the `in_net` / `out_net` role (probability only). Share of frames whose voice
probability reaches the threshold; speech = clean frame RMS >= clean RMS - 15 dB, quiet speech =
clean RMS - 40 .. - 15 dB (syllable onsets and tails, where a gate cuts), keys only at -20 dBFS.
`vad[t]` paired with the frame the model targets (rnnoise2: one frame later than the input).

| Threshold | Model           | Speech 0 dB % | Quiet 0 dB % | Speech -10 dB % | Quiet -10 dB % | Keys only % |
| --------- | --------------- | ------------- | ------------ | --------------- | -------------- | ----------- |
| 0.02      | nnnoiseless 0.5 | 96.2          | 45.7         | 96.2            | 66.0           | 15.2        |
| 0.05      | nnnoiseless 0.5 | 94.4          | 30.2         | 93.7            | 47.5           | 7.8         |
| 0.1       | nnnoiseless 0.5 | 92.6          | 24.7         | 91.4            | 32.7           | 4.2         |
| 0.2       | nnnoiseless 0.5 | 89.6          | 17.9         | 87.1            | 22.2           | 2.0         |
| 0.3       | nnnoiseless 0.5 | 85.8          | 15.4         | 81.7            | 14.8           | 1.3         |
| 0.5       | nnnoiseless 0.5 | 81.7          | 11.7         | 75.6            | 10.5           | 0.4         |
| 0.7       | nnnoiseless 0.5 | 78.1          | 7.4          | 69.5            | 4.9            | 0.0         |
| 0.9       | nnnoiseless 0.5 | 69.8          | 4.9          | 56.4            | 3.1            | 0.0         |
| 0.02      | rnnoise2 Full   | 100.0         | 78.4         | 98.7            | 72.8           | 33.3        |
| 0.05      | rnnoise2 Full   | 100.0         | 69.1         | 98.5            | 67.3           | 19.1        |
| 0.1       | rnnoise2 Full   | 99.8          | 64.2         | 98.2            | 63.6           | 11.3        |
| 0.2       | rnnoise2 Full   | 99.5          | 58.6         | 98.0            | 56.2           | 5.2         |
| 0.3       | rnnoise2 Full   | 99.3          | 54.3         | 97.7            | 51.9           | 3.1         |
| 0.5       | rnnoise2 Full   | 99.0          | 47.5         | 96.5            | 44.4           | 1.3         |
| 0.7       | rnnoise2 Full   | 97.2          | 38.3         | 94.1            | 30.9           | 0.5         |
| 0.9       | rnnoise2 Full   | 94.6          | 21.0         | 90.1            | 15.4           | 0.0         |
| 0.02      | rnnoise2 Little | 99.7          | 74.7         | 97.9            | 77.2           | 52.9        |
| 0.05      | rnnoise2 Little | 99.3          | 67.3         | 97.0            | 73.5           | 43.8        |
| 0.1       | rnnoise2 Little | 99.2          | 64.2         | 96.9            | 67.3           | 36.5        |
| 0.2       | rnnoise2 Little | 98.7          | 58.6         | 96.5            | 58.0           | 28.2        |
| 0.3       | rnnoise2 Little | 97.9          | 54.3         | 96.5            | 54.3           | 24.9        |
| 0.5       | rnnoise2 Little | 97.4          | 45.1         | 95.9            | 40.7           | 18.3        |
| 0.7       | rnnoise2 Little | 96.4          | 38.3         | 94.6            | 30.2           | 9.0         |
| 0.9       | rnnoise2 Little | 94.2          | 25.9         | 89.4            | 14.2           | 1.6         |

Reading: at the same false-alarm rate as the old detector at its current threshold 0.5 (0.4–0.5 %
of keys-only frames), `Full` at 0.7 hears 94–97 % of speech frames instead of 76–82 % and 31–38 %
of quiet speech frames instead of 10–12 %. The gate's keep threshold (old 0.05: 7.8 % of keys-only
frames) maps to 0.2 on `Full` (5.2 %), which still hears 56–59 % of quiet speech instead of 30–48 %.
For a gate whose first rule is "never cut speech, rather let a key through", that is the useful part
of this model; the filter itself (gains) is not. The mic chain uses exactly this (`ai_denoiser.rs`
`in_net`, `IN_NET_VOICE_PROB` 0.7 / `IN_NET_KEEP_PROB` 0.2, DEVLOG milestone 35).

### Cross-check against the upstream C

Is the model worse, or the port? `report_vs_upstream_c` runs the same 6.3 inputs (i16-rounded, as
`rnnoise_demo` reads them) through the upstream C demo (`dump_report_mixes` writes the inputs, the
demo in `.claude/tmp/rnnoise2/build/` is run by hand, see the test's doc comment) and through this
port, both outputs quantized to i16 like the demo's `(short)` cast, then computes the same metrics
(VAD is not available from the demo). Speech at -20 dBFS, ~10.6 s per mix, 30 s of keys:

| Input      | Model  | Output     | Pause supp. dB | Far pause supp. dB | Loud lost % | Soft lost % | Mean loss dB | Lost >6 dB % | Soft syll. lost % | Port vs C max / RMS [LSB]  |
| ---------- | ------ | ---------- | -------------- | ------------------ | ----------- | ----------- | ------------ | ------------ | ----------------- | -------------------------- |
| 0 dB SNR   | Full   | upstream C | 39.6           | 45.2               | 0.0         | 0.9         | 1.77         | 6.5          | 30.2              |                            |
| 0 dB SNR   | Full   | rnnoise2   | 39.5           | 45.0               | 0.0         | 0.9         | 1.75         | 6.5          | 30.2              | 89 / 2.93 (3 wrapped in C) |
| 0 dB SNR   | Little | upstream C | 34.6           | 68.0               | 0.0         | 3.4         | 2.38         | 6.5          | 32.7              |                            |
| 0 dB SNR   | Little | rnnoise2   | 34.6           | 68.0               | 0.0         | 3.4         | 2.37         | 6.5          | 32.7              | 112 / 3.20 (9 wrapped in C) |
| -10 dB SNR | Full   | upstream C | 38.5           | 41.0               | 0.7         | 5.6         | 4.88         | 12.7         | 37.7              |                            |
| -10 dB SNR | Full   | rnnoise2   | 38.6           | 41.0               | 0.7         | 5.6         | 4.87         | 12.7         | 37.7              | 118 / 4.28                 |
| -10 dB SNR | Little | upstream C | 38.4           | 51.2               | 0.3         | 6.3         | 5.62         | 11.2         | 38.3              |                            |
| -10 dB SNR | Little | rnnoise2   | 38.4           | 51.2               | 0.3         | 6.3         | 5.63         | 11.2         | 38.9              | 95 / 4.28                  |
| keys only  | Full   | upstream C | 34.3           |                    |             |             |              |              |                   |                            |
| keys only  | Full   | rnnoise2   | 34.3           |                    |             |             |              |              |                   | 21 / 0.56                  |
| keys only  | Little | upstream C | 42.3           |                    |             |             |              |              |                   |                            |
| keys only  | Little | rnnoise2   | 42.3           |                    |             |             |              |              |                   | 20 / 0.39                  |

Every metric agrees within 0.2 dB / 0.6 %: the quiet-syllable loss and the speech loss under keys
are properties of the upstream model, not of the port. The sample-wise difference grows with clip
length (up to 118 LSB, about 4 LSB RMS after 10 s) because int8 rounding flips in the recurrent
state accumulate; the upstream NEON build differs from the generic C by the same order (90 LSB on
2 s). "Wrapped in C": the demo's `(short)` cast wraps samples beyond the i16 range; the port
saturates. The numbers differ slightly from the main table above because these inputs are
i16-rounded and the outputs i16-quantized.

## Cost

`rnnoise2_bench` (spec 6.4): `--release`, 10 s (1000 frames) of the 0 dB keys mix, mean per
frame, macOS arm64 (Apple Silicon), single thread, best of three runs. A ratio only; Windows
numbers come with the integration (spec chapter 8).

| Model           | before [ns / frame] | after [ns / frame] | after, % of real time | `voice_probability` [ns / frame] |
| --------------- | ------------------- | ------------------ | --------------------- | -------------------------------- |
| nnnoiseless 0.5 | 39 244              | 29 866             | 0.30                  | -                                |
| rnnoise2 Full   | 414 321             | 82 138             | 0.82                  | 75 495                           |
| rnnoise2 Little | 188 408             | 57 823             | 0.58                  | 51 548                           |

Output is bit-identical before and after (every sample and VAD value of the three reference clips,
both models, debug and release). What changed, per-layer timing from `nnet::tests::stage_timing`
(`--release --lib stage_timing -- --ignored --nocapture`):

| Stage (Full)                   | before [µs] | after [µs] | Change                                                                                 |
| ------------------------------ | ----------- | ---------- | -------------------------------------------------------------------------------------- |
| 6 GRU int8 mat-vecs (384→1152) | 6 x 58      | 6 x 7      | `kernel.rs`: flat block list, i32 accumulation, `sdot`/AVX2                            |
| conv2 int8 (384→384)           | 19.7        | 2.7        | same kernel                                                                            |
| `dense_out` f32 (1536→32)      | 5.7         | 1.9        | fixed-width accumulators in registers (same order per output)                          |
| conv1 f32 (195→128)            | 1.3         | 0.8        | same                                                                                   |
| coarse pitch xcorr             | -           | -          | 8 lags side by side, each still one sequential sum: about -6 µs per frame in the bench |

`compute_rnn` went from 374 µs to 52 µs (Little: 145 → 28). The rest of a frame (two forward
FFTs, pitch search, band energies; plus pitch filter, gains, inverse FFT for `process_frame`) is
now 30 µs and dominates `voice_probability`. Kernel choice is made once per model load
(`Kernel::detect`); without AVX2 / `dotprod` the scalar i32 kernel runs (Full about 275 µs here).
The input quantization avoids `f64::floor`, a libm call per value on x86_64 without SSE4.1 (the
Windows baseline). The AVX2 path is compiled and clippy-checked for `x86_64-apple-darwin`
(`-Zbuild-std`) but not run on this machine; `kernel::tests::simd_matches_scalar` covers it on
any AVX2 CPU.
