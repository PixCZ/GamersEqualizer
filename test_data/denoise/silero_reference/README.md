# Silero VAD reference

For `crates/vendor/silero_vad/tests/reference.rs`. Made by
`crates/vendor/silero_vad/tools/convert_weights.py`.

| File         | Content                                                                                     |
| ------------ | ------------------------------------------------------------------------------------------- |
| `in_16k.f32` | 10 s at 16 kHz, raw little-endian f32: `speech_clean.wav` at -20 dBFS under the ESC-50 keys at 0 dB SNR |
| `probs.f32`  | onnxruntime's probability per 512-sample chunk (upstream `silero_vad_16k_op15.onnx`, v6.2.3) |

`in_16k.f32` is derived from the non-commercial fixtures one folder up: the same licence note applies.
