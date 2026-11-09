"""Silero VAD ONNX (16 kHz, `silero_vad_16k_op15.onnx` of tag v6.2.3) -> models/silero_vad_v6.bin,
and the reference for tests/reference.rs. Needs numpy, onnx, onnxruntime and scipy in a venv.

  python convert_weights.py <silero_vad_16k_op15.onnx> <mix_48k.wav> <repo root>

Blob: magic "SVAD6\0\0\0", then per tensor (fixed order below) a u32 count and that many
little-endian f32. The `.safetensors` file in the same upstream folder holds other weights (its
output differs by up to 0.46 from the ONNX models); do not use it.

Reference: the first 10 s of <mix_48k.wav> (the shared speech over the ESC-50 keys, 0 dB SNR,
as `speech_over_keyboard(-20, 0)` in ai_denoiser.rs writes it) resampled to 16 kHz, and
onnxruntime's probability for each 512-sample chunk.
"""
import hashlib, os, struct, sys
import numpy as np
import onnx
import onnxruntime as ort
from onnx import numpy_helper
from scipy.io import wavfile
from scipy.signal import resample_poly

ORDER = [
  "model.stft.forward_basis_buffer",
  "model.encoder.0.reparam_conv.weight", "model.encoder.0.reparam_conv.bias",
  "model.encoder.1.reparam_conv.weight", "model.encoder.1.reparam_conv.bias",
  "model.encoder.2.reparam_conv.weight", "model.encoder.2.reparam_conv.bias",
  "model.encoder.3.reparam_conv.weight", "model.encoder.3.reparam_conv.bias",
  "model.decoder.rnn.weight_ih", "model.decoder.rnn.weight_hh",
  "model.decoder.rnn.bias_ih", "model.decoder.rnn.bias_hh",
  "model.decoder.decoder.2.weight", "model.decoder.decoder.2.bias",
]

model_path, wav_path, root = sys.argv[1:4]
init = {i.name: numpy_helper.to_array(i) for i in onnx.load(model_path).graph.initializer}
blob = b"SVAD6\0\0\0"
for name in ORDER:
  t = init[name].astype("<f4").ravel()
  blob += struct.pack("<I", t.size) + t.tobytes()
out = os.path.join(root, "crates/vendor/silero_vad/models/silero_vad_v6.bin")
open(out, "wb").write(blob)
open(out + ".sha256", "w", newline="\n").write(hashlib.sha256(blob).hexdigest() + "\n")
print(len(blob), "bytes, source sha256", hashlib.sha256(open(model_path, "rb").read()).hexdigest())

sr, x = wavfile.read(wav_path)
assert sr == 48000
x = resample_poly(x.astype(np.float32), 1, 3).astype("<f4")[: 16000 * 10]
sess = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
state, ctx, probs = np.zeros((2, 1, 128), np.float32), np.zeros(64, np.float32), []
for i in range(0, len(x) - 511, 512):
  chunk = x[i:i + 512]
  y, state = sess.run(None, {"input": np.concatenate([ctx, chunk])[None, :], "state": state, "sr": np.array(16000, np.int64)})
  ctx = chunk[-64:]
  probs.append(y[0, 0])
ref = os.path.join(root, "test_data/denoise/silero_reference")
x.tofile(os.path.join(ref, "in_16k.f32"))
np.array(probs, "<f4").tofile(os.path.join(ref, "probs.f32"))
print(len(probs), "chunks, probability", float(min(probs)), "..", float(max(probs)))
