# Denoiser test fixtures

Used by the tests in `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs`. Test-only, never shipped.

| File                 | Content                    | Source                                                                        |
| -------------------- | -------------------------- | ----------------------------------------------------------------------------- |
| `speech_clean.wav`   | Clean speech, 48 kHz mono  | DeepFilterNet repo `assets/clean_freesound_33711.wav` (same pinned commit)    |
| `1-*-A-32.wav`       | Keyboard typing, 44.1 kHz  | [ESC-50](https://github.com/karolpiczak/ESC-50), class `keyboard_typing`      |
| `1-*-A-31.wav`       | Mouse clicks, 44.1 kHz     | [ESC-50](https://github.com/karolpiczak/ESC-50), class `mouse_click`          |

ESC-50 clips are licensed CC BY-NC 3.0 (attribution: Karol J. Piczak, ESC-50 dataset).
