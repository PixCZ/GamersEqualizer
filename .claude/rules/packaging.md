---
paths:
  - "scripts/package.ps1"
  - "scripts/dev-rebuild.ps1"
  - "scripts/sign-binaries.ps1"
  - "scripts/create-dev-cert.ps1"
  - "scripts/cpu-bench-package.ps1"
  - "packaging/**"
  - "crates/build-support/**"
  - ".cargo/config.toml"
  - ".github/workflows/**"
  - "crates/gamersequalizer-daemon/src/lifecycle.rs"
  - "crates/gamersequalizer-daemon/src/**/takeover.rs"
  - "crates/gamersequalizer-daemon/src/win_audio/apo_install/location.rs"
---

# Tester package, install layout, versions, signing

ZIP for testers on other PCs (no setup.exe; the app installs/uninstalls itself and its system parts):
`gamersequalizer.exe` + `gamersequalizer_apo.dll` + `LICENSE.txt` (GPL-3.0); what testers need to know is in
`README.md`.

- Installed layout: the installer (one admin prompt) copies the exe to `%ProgramFiles%\GamersEqualizer\`
  next to `apo\gamersequalizer_apo.dll`. A copy started from elsewhere then starts the installed one
  (`handover_target` in `apo_install/location.rs`), and "start with Windows" points there. A newer copy ends
  an older running one first (`takeover.rs`), so an update is "unpack, start, click Update". A dev build
  (`target\<profile>\`) runs in place and is never installed. User data stays in `%APPDATA%\GamersEqualizer`.
- The installer registers the app in "Add or remove programs" (`HKLM\...\Uninstall\GamersEqualizer`,
  `DisplayVersion` = the build's version, which winget compares) and adds a Start-menu shortcut; the
  uninstaller removes both and moves its own locked exe to `%TEMP%` when run from it.
- winget: draft manifests in `packaging/winget/PixCZ.GamersEqualizer/` (zip with nested exe,
  `elevatesSelf`); URL and SHA-256 are filled from the GitHub release at submission.
- `scripts/package.ps1`: final build from scratch (`dist` + `apo-dist` profiles, fat LTO, built at the same time, minutes; verifies
  the rustc flags in the verbose log). `-Fast` packages the `release` build instead (seconds);
  `scripts/dev-rebuild.ps1 -Package` does that after the dev build.
- Output: `target/package/GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>[-dirty][-fast].zip`.
  Old ZIPs are never deleted or overwritten; only `target/package/staging` is cleared.
- Version (`crates/build-support/version_info.rs`): a GitHub build of a tag (`0.1.0` or `v0.1.0`) shows
  that version in the About card, logs and the exe/DLL details; it fails on a tag that is not `X.Y.Z`.
  Every other build shows the short commit hash, `-dirty` when the sources differ from it; `<version>`
  in the ZIP name is then `dev`. The `version` in the Cargo.toml files is not shown anywhere.
- The C runtime is linked statically (`.cargo/config.toml`), so testers need no Visual C++ Redistributable;
  `package.ps1` fails if a binary still imports `VCRUNTIME140.dll`. The same file turns on Control
  Flow Guard for every build; `package.ps1` fails if a binary lacks it or exports `VSTPluginMain`.
- Never package a `-DebugCapture` APO (`dev-rebuild.ps1` skips `-Package` with it).
- Binaries are unsigned or signed with our self-signed certificate. Testers install no certificate (neither
  SmartScreen nor Smart App Control accepts a local root): SmartScreen "More info > Run anyway"; Smart App
  Control blocks them entirely. `scripts/create-dev-cert.ps1` makes a password-protected end-entity
  code-signing certificate (1 year) in `secrets\`; `package.ps1 -Sign` signs with a mandatory timestamp. In CI
  a separate step without cargo signs the finished binaries (secrets `SIGNING_CERT_PFX_BASE64` /
  `SIGNING_CERT_PASSWORD`).
