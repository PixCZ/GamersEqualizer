# deep_filter (fork)

DeepFilterNet 3 inference for the mic chain (`gamersequalizer-mic-vst`, `ai_denoiser.rs`).

| What      | Value                                                                                       |
| --------- | ------------------------------------------------------------------------------------------- |
| Upstream  | [Rikorose/DeepFilterNet](https://github.com/Rikorose/DeepFilterNet) `libDF`, rev `d375b2d`  |
| License   | MIT OR Apache-2.0 (`LICENSE`, `LICENSE-MIT`, `LICENSE-APACHE`, unchanged from upstream)      |
| Model     | `models/DeepFilterNet3_onnx.tar.gz` from the same rev, embedded with `include_bytes!`        |
| Model hash| SHA-256 `c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616`                    |

## Why a fork

Upstream pins tract `^0.21.4` (and breaks on newer 0.21.x), while DPDFNet already runs on tract 0.23.
Keeping both meant two tract versions in the APO DLL, and DFN3 on the slower one.

## Changes against upstream

- Only `lib.rs`, `transforms.rs` (without the rubato resampling), `logging.rs` and `tract.rs` are kept. Dataset, training, C API,
  WASM, binaries and the upstream tests are left out. The features are reduced to `tract` and
  `default-model`.
- `tract.rs` is ported to tract 0.23 (API renames only, see the git history of this file).
- The model path points at `models/` in this directory.

Check after any change to this crate or to tract: `ai_denoiser::tests::dfn3_reference` compares the
output against a saved reference.
