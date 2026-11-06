# Embedded noise suppression models

Compiled into the mic DSP (`include_bytes!` in `src/dpdfnet.rs`), so the APO inside audiodg never
reads model files from disk.

| File                      | Model                         | Source                                                                                       | License    |
| ------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------- | ---------- |
| `dpdfnet2_48khz_hr.onnx`  | DPDFNet-2, 48 kHz, causal     | [Ceva-IP/DPDFNet](https://huggingface.co/Ceva-IP/DPDFNet) `onnx/dpdfnet2_48khz_hr.onnx`      | Apache-2.0 |

SHA-256 `7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b`, 10 493 337 bytes.
Code: [ceva-ip/DPDFNet](https://github.com/ceva-ip/DPDFNet) (Apache-2.0); paper arXiv:2512.16420.
