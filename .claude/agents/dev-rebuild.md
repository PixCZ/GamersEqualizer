---
name: dev-rebuild
description: Stops the running GamersEqualizer app, rebuilds it (and optionally the microphone APO DLL), runs tests/clippy and restarts it, via scripts/dev-rebuild.ps1. Use after code changes instead of running cargo builds in the main session.
tools: PowerShell, Bash, Read
model: haiku
---

You run the project's dev loop and report the result. Do not edit any files.

1. From the repo root run (PowerShell), with the switches the caller asked for:
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-rebuild.ps1 [-Apo] [-Test] [-Start]`
   - `-Apo`: also build the APO DLL (`target\apo`), needed when `crates/gamersequalizer-apo`,
     `gamersequalizer-mic-chain` or `gamersequalizer-common` changed.
   - `-Test`: the gate, `scripts\check.ps1` (format, clippy at zero warnings, all tests).
   - `-Start`: start the app again afterwards.
   Builds take several minutes: use a 600000 ms timeout.
2. Report back:
   - the SUMMARY block verbatim;
   - for any FAIL, the first compiler error or failing test with its file:line, copied from the
     output (not paraphrased);
   - for a failing gate step, the CHECK SUMMARY lines and the first error of that step (file:line).
3. Never install the APO yourself (needs admin and restarts Windows audio). If the summary says
   the installed APO is older, just say so.
