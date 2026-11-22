---
paths:
  - "scripts/apo-*.ps1"
  - "scripts/tests/**"
  - "crates/gamersequalizer-daemon/src/win_audio/apo_install/**"
  - "crates/gamersequalizer-daemon/src/health/**"
  - "crates/gamersequalizer-daemon/src/setup_cli.rs"
  - "crates/gamersequalizer-daemon/src/ui/setup*"
  - "crates/gamersequalizer-apo/src/event_log.rs"
---

# APO installer and its diagnostics

- The APO is installed via the app's banner or `scripts/apo-install.ps1`. Both need admin and restart
  Windows audio, so a human runs them.
- A third start, for an update only: the tray menu's and the tray flyout's fix for outdated effects
  (`HealthMonitor::start_update`) runs the same installer on the active endpoints that already carry
  our effects, never a first install. A second start within `UPDATE_GUARD` opens the window instead.
- "Installed but does nothing": the app log alone must tell why; testers send only the log, no extra
  tools. `[ApoCheck]`: Windows build, DLL, COM registration, every device with our effects (slots, state)
  and the devices in use. `[ApoNote]`: the APO's own notes from inside audiodg (Application log, source
  `GamersEqualizer`, `gamersequalizer-apo/src/event_log.rs`; each names the process and the DLL build)
  plus audiodg crashes, Defender detections and Code Integrity events about our files, and every line
  the installer printed (`installer: ...`, written to the same event source). `[ApoLink]`: why
  the service cannot reach the APO. The window learns the verdict via `run_status.json`.
- The installer writes nothing to its window while Windows audio is stopped (`Say` in
  `apo-install.ps1` holds those lines): a click into a console window pauses the script at its next
  output until a key is pressed, which once left a PC without sound.
- Windows versions: the installer needs Windows 10 version 1803 or newer (composite effect lists). On an
  older Windows it stops before anything changes; the app checks the build before it asks for admin. An
  endpoint whose driver effects it could not keep (`Get-EndpointBlocker`) is never touched: it is skipped
  and named, and when none is left nothing changes. Self-check of that rule:
  `scripts/tests/apo-install.tests.ps1` (part of `-Test`, also runs under `pwsh` on macOS). Developed and
  tested on Windows 11; Windows 10 only lightly tested, no guarantee there (fewer audio capabilities).
- Devices: the setup card installs on the microphone and output in use; other connected devices are
  opt-in checkboxes. A device whose install the installer rolled back is not offered again until "Try
  again". Before changing an endpoint the installer saves its effect lists to
  `%ProgramFiles%\GamersEqualizer\apo-backup\`: again on each install while the endpoint holds none of
  our effects (a driver update or a rollback may have changed them), kept while ours are in. The
  uninstaller restores them, then removes our effects.
- It moves the driver's legacy effect values into composite lists only in the slots ours go into (a
  legacy value shadows only its own slot). A device that does not open during the self-test is opened
  once more without our effects: if it opens then, ours stay out; if not (exclusive use, privacy), ours
  go back in unchecked, unless driver effects were moved for them. An endpoint without `FxProperties`
  (inbox "High Definition Audio Device", VMs; Administrators may not create subkeys there) gets the key,
  made with the backup/restore privileges so no owner or ACL changes; its backup is marked `Created`,
  and rollback and uninstall delete the key.
- Mic effect slot: the installer puts the mic chain in EFX, then opens the mic briefly
  (`gamersequalizer.exe --probe-mic {guid}`) and moves it to MFX when Windows did not start it there
  (some notebook mic arrays). The Mic tab offers the same switch later ("Try another effect slot").
- Without a window (`setup_cli.rs`, for winget/scripts): `--install` installs on the endpoints that
  already carry our effects, else on the devices in use, else the app alone (`-AppOnly`, no audio
  device); `--uninstall` ends the other copies, then uninstalls. `--silent` hides the elevated window
  and drops `-FromApp` (no key prompts). The process waits and exits with the script's exit code.
- `apo-install.ps1` / `apo-uninstall.ps1` are built into the exe (`include_str!` in
  `win_audio/apo_install/`) and written to `%LOCALAPPDATA%\GamersEqualizer\scripts` before each
  elevated run. What both share is in `scripts/apo-common.ps1`: run from disk they dot-source it
  (the `# @common` line), the app puts its text in that line's place before writing and hashing.
  The DLL stays a separate file next to the exe (audiodg loads it from disk).
