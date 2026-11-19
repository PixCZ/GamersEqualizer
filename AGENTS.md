# AGENTS.md

Guide for coding agents (Claude Code, Codex, Copilot, ...) working in this repo.
`CLAUDE.md` only imports this file; detailed rules live in `.claude/rules/`. Claude Code loads
`installer.md`, `packaging.md` and `release.md` there only when it works on their files; other agents
read them when they touch the installer, packaging or a release.

## Project

Audio control center for Windows, built for gamers: per-app mixer with ChatMix, headphone correction
EQ, game profiles, sidetone and a low-latency microphone chain (echo cancellation, AI noise
suppression, voice leveler). The processing runs as Windows audio effects (APOs) on the user's real
devices: no virtual sound card, no kernel driver, no network code. Pure Rust with a native UI
(`eframe` / `egui`), Windows 10 1803+ / Windows 11, x64.

What users see is in `README.md`; history, measurements and what was tried and dropped are in
`DEVLOG.md`. `SPECIFICATION.md` is the original brief, kept as written: where it differs, the code wins.

Three places our code runs (details in `.claude/rules/architecture.md`):

| Where                              | What                                                                   |
| ---------------------------------- | ---------------------------------------------------------------------- |
| `audiodg.exe` (Windows audio)      | `gamersequalizer_apo.dll`: mic chain, output EQ, per-app stream EQ     |
| `gamersequalizer.exe --daemon`     | Background service: devices, mixer, game detection, health, sidetone   |
| `gamersequalizer.exe`              | The window (egui); closing it leaves the service running               |

They talk through shared memory (`crates/gamersequalizer-common/src/shm/`) and JSON files in
`%APPDATA%\GamersEqualizer`.

Core properties: **the running app never crashes, and our DLL never takes Windows audio down**; the
binary has **no network code**; latency of the microphone chain is measured and shown, never guessed.

## Structure

| Path                                | Purpose                                                                   |
| ----------------------------------- | ------------------------------------------------------------------------- |
| `crates/gamersequalizer-common/`    | Shared by all: shared-memory layout, biquads, limiter, profiles, sidetone |
| `crates/gamersequalizer-mic-chain/` | Microphone DSP chain (AEC, noise suppression, leveler), a plain library   |
| `crates/gamersequalizer-apo/`       | The APO DLL: COM object, mic / output / stream processors; probe examples |
| `crates/gamersequalizer-daemon/`    | `gamersequalizer.exe`: service loop and window (see below)                |
| `crates/vendor/`                    | Forks: DeepFilterNet, RNNoise, Silero VAD, egui-wgpu; outside our lints   |
| `crates/build-support/`             | Version info shared by the build scripts                                  |
| `scripts/`                          | Dev rebuild, packaging, signing, APO install / uninstall and its tests    |
| `test_data/`                        | Test fixtures; `test_data/private/` is git-ignored (own recordings)       |
| `docs/`                             | Build guide, AEC3 tuning notes, screenshots                               |
| `packaging/`                        | winget manifest drafts, release notes                                     |
| `.claude/rules/`                    | Detailed agent rules: architecture, style, testing, installer, release    |
| `secrets/`                          | Never read, edit, or commit                                               |

Inside `crates/gamersequalizer-daemon/src/`:

| Path              | Purpose                                                                         |
| ----------------- | ------------------------------------------------------------------------------- |
| `main.rs`         | Argument dispatch, starting the window                                          |
| `daemon.rs`       | The service loop (`--daemon`)                                                   |
| `lifecycle.rs`    | Handover to the installed copy, import after the window closes, service mutex   |
| `shm_mapping.rs`  | The window <-> service shared-memory mapping                                    |
| `win_audio/`      | Windows audio (WASAPI, COM), APO install and link, mixer, routing, sidetone     |
| `output_eq/`      | Headphone / game / chat / media EQ layers, game and genre detection, AutoEQ     |
| `sonar_importer/` | Import of SteelSeries Sonar profiles                                            |
| `ui/`             | The egui window: one file per tab, shared widgets, theme                        |
| `health/`         | Checks behind the warnings and the `[ApoCheck]` / `[ApoNote]` log lines         |
| `i18n*.rs`        | UI texts: the English text is the key, `i18n_cs.rs` is the Czech table          |

## Working mode: write less code

From [Ponytail](https://github.com/DietrichGebert/ponytail) (its skill is authoritative when installed):
map every place a change must reach before writing; take the smallest complete option (skip what wasn't
asked, reuse, stdlib, existing deps, then minimal new code); fix bugs once at the root in shared code;
moved code keeps its validation; a shortcut gets a comment with its limit. Never cut trust-boundary
validation, data-loss handling, security, accessibility or anything asked for. End with 1–2 lines on
what was skipped, unchecked or risky.

Navigating code: CodeGraph (MCP server in `.mcp.json`); prefer `codegraph_explore` over grep + reading
whole files. After large refactors `npm run graph:sync`; first time `npm run graph:init`.

## Delegation (Claude Code): cheap models run, the main session decides

Output of builds, tests, benchmarks and logs never goes into the main context. A Haiku subagent runs it
and reports the result; the main session works from that summary and reads output itself only for a
few known lines.

| Work                                                                                   | Agent         |
| -------------------------------------------------------------------------------------- | ------------- |
| Build, the gate (`check.ps1`), tester ZIP, restart the app (`dev-rebuild.ps1`)         | `dev-rebuild` |
| Any other command with long output: `cargo check` / `test` / `clippy` of one crate,    | `cmd-runner`  |
| benchmarks and `#[ignore]`d reports, `cpu-bench-package.ps1`, `cargo audit`,           |               |
| `cargo update --dry-run`, probe and `debug_capture` examples, vendor crate tests       |               |
| Reading a log (app log, event log, saved build output, `vars.tsv` / `info.txt`)        | `log-analyst` |

Give the agent the exact command and what to report (the error, which numbers). Commands that need
admin or restart Windows audio are never delegated: a human runs them.

## Dev loop

- `scripts/check.ps1` is the gate: format, clippy at zero warnings (also with `debug-capture`), workspace
  tests, the installer self-check (also under Windows PowerShell 5.1, which runs the installer on users'
  PCs) and the vendor crates' tests. `dev-rebuild.ps1 -Test` runs it, and so does CI
  (`.github/workflows/ci.yml`) on every push. Work is done only when it passes.
- `scripts/dev-rebuild.ps1 [-Apo] [-Test] [-Package] [-Start]`: stops the running app, builds it (and with
  `-Apo` also the microphone APO DLL), runs the gate (`-Test`), makes a tester ZIP (`-Package`) and
  starts the app again.
- Debug capture: `-Apo -DebugCapture` builds the APO with the `debug-capture` feature. After installing it,
  `cargo run --release -p gamersequalizer-apo --example debug_capture -- [seconds]` records up to 60 s of
  every mic-chain stage (WAV) plus per-block variables (`vars.tsv`, `info.txt`) into
  `test_data/private/captures/` (git-ignored). Never ship an APO built with it.
- Automatic ChatMix offline: `cargo run --release -p gamersequalizer-common --example duck_sim -- game.wav
  chat.wav out_dir [duck_tuning.json] [--strength 0.5] [--pair-delay 3] [--mix]`
  runs two 48 kHz recordings through the stream APOs' ducking path (Silero on the chat, pairing after a
  delay), or with `--mix` through the service's path for games the stream effects cannot see, writes the
  ducked game and both mixes and prints one line per talk spurt. It does not model the chat stream's EQ
  before the detector or more than one chat and one game stream. The service reads the same
  `duck_tuning.json` from `%APPDATA%\GamersEqualizer` and hands it to the running APOs (names and ranges:
  `ducking::DuckTuning`), so tuning never needs a new APO install.
- The APO is installed via the app's banner or `scripts/apo-install.ps1`; both need admin and restart
  Windows audio, so a human runs them. When it is installed but does nothing, the app log alone must say
  why (`[ApoCheck]`, `[ApoNote]`, `[ApoLink]`); installer and diagnostics rules: `.claude/rules/installer.md`.
- Tester ZIP, installed layout, versions and signing: `.claude/rules/packaging.md`.

## Before a release or a push to main

Offer the user the dependency check (GitHub Actions pins, crates and `cargo audit`, the tract and
egui-wgpu forks, the toolchain) and write release notes as in `.claude/rules/release.md`. Offer it,
don't run it unasked.

## General

- No global installs (`npm -g`, `pip install` outside a venv). Dependencies go into the project; system tools via `winget`, and ask first.
- Don't add a dependency when the stdlib or an existing dep does the job.
- Don't touch `secrets/` or `.env*`. Delete only tracked files unless explicitly told otherwise.
- Markdown tables: pad cells so columns line up in plain text.
- Follow `.claude/rules/*.md` for style, testing and API conventions.
