## Lighter on your CPU

This release makes the microphone cheaper to run. The AI noise suppression needs noticeably less of
your processor, the microphone works less while you are silent, and the pauses between your words
stay clean. Your voice sounds the same as before.

- **AI noise suppression costs less.** DeepFilterNet needs about an eighth to a sixth less of the
  processor, with the same sound.
- **Typing without talking costs less.** While nobody speaks, the microphone skips work it only needs
  for your voice.
- **Pauses stay silent.** With DeepFilterNet or DPDFNet, long pauses no longer carry a short burst of
  hiss every few seconds.
- **The CPU number says what it means.** The Mic tab shows the load as a share of one core and of the
  whole processor, so it no longer looks several times higher than in Task Manager.

### Updating

Unpack the ZIP, start `gamersequalizer.exe` and click **Update**. Your settings and profiles stay.
