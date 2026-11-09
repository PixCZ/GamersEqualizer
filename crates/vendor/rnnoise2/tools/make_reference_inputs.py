#!/usr/bin/env python3
"""Make the three reference inputs in test_data/denoise/rnnoise2_reference/.

Usage (from the repo root):
  python3 -I crates/vendor/rnnoise2/tools/make_reference_inputs.py

Writes 48 kHz mono 16-bit WAVs, 2.25 s each (225 frames of 480 samples, an exact
multiple so upstream's rnnoise_demo processes every frame):
  in_speech.wav  speech_clean.wav from 1.00 s (speech onset at ~1.2 s, short pause at ~3.2 s)
  in_keys.wav    the same speech + keyboard typing (1-137-A-32.wav from 0.80 s) at -10 dB SNR
  in_clicks.wav  0.25 s of digital silence (exercises the E < 0.04 path), then mouse clicks
                 (1-118206-A-31.wav from 0.30 s)

Python stdlib only, deterministic.
"""

import array
import math
import os
import sys
import wave

SR = 48000
FRAME = 480
FRAMES = 225
N = FRAME * FRAMES
SRC_DIR = os.path.join("test_data", "denoise")
OUT_DIR = os.path.join(SRC_DIR, "rnnoise2_reference")


def read_wav(name):
  with wave.open(os.path.join(SRC_DIR, name)) as w:
    if w.getnchannels() != 1 or w.getsampwidth() != 2:
      sys.exit(f"{name}: expected mono 16-bit")
    return w.getframerate(), [float(v) for v in array.array("h", w.readframes(w.getnframes()))]


def write_wav(name, samples):
  pcm = array.array("h", (max(-32768, min(32767, int(round(v)))) for v in samples))
  with wave.open(os.path.join(OUT_DIR, name), "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(SR)
    w.writeframes(pcm.tobytes())


def cut_48k(name, start_s, length):
  """`length` samples at 48 kHz from `start_s` seconds into the file.

  shortcut: ESC-50 clips are 44.1 kHz and resampled by linear interpolation (aliasing and a
  slight HF droop). Fine for a test input; use a proper resampler if the input itself matters.
  """
  sr, x = read_wav(name)
  out = []
  for i in range(length):
    pos = (start_s + i / SR) * sr
    k = int(math.floor(pos))
    frac = pos - k
    if k + 1 >= len(x):
      sys.exit(f"{name}: clip too short for the requested cut")
    out.append(x[k] * (1.0 - frac) + x[k + 1] * frac)
  return out


def power(x):
  return sum(v * v for v in x) / len(x)


def main():
  os.makedirs(OUT_DIR, exist_ok=True)
  speech = cut_48k("speech_clean.wav", 1.0, N)
  keys = cut_48k("1-137-A-32.wav", 0.8, N)
  clicks = cut_48k("1-118206-A-31.wav", 0.3, N - SR // 4)

  # -10 dB SNR over the whole clip (speech power includes its pauses).
  g = math.sqrt(power(speech) / power(keys) * 10.0)
  mix = [s + g * k for s, k in zip(speech, keys)]
  # Scale speech and mix by the same factor so the mix peaks at -1 dBFS without clipping.
  scale = min(1.0, 32767 * 10 ** (-1 / 20) / max(abs(v) for v in mix))
  write_wav("in_speech.wav", [v * scale for v in speech])
  write_wav("in_keys.wav", [v * scale for v in mix])
  write_wav("in_clicks.wav", [0.0] * (SR // 4) + clicks)
  print(f"keyboard gain {g:.3f}, common scale {scale:.4f}")


if __name__ == "__main__":
  main()
