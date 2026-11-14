# AGENTS.md

Guide for coding agents (Claude Code, Codex, Copilot, ...) working in this repo.
`CLAUDE.md` only imports this file; detailed rules live in `.claude/rules/`.

## Project

Desktop app for AI-assisted creative work (images, video, 3D), similar in shape to
[ArtCraft](https://github.com/storytold/artcraft): Tauri 2 + Rust backend, React/TypeScript frontend,
SQLite for local state. _TBD: product specifics from the brief._

Core property: **the running app never crashes** — see `.claude/rules/architecture.md`.
Out of scope: browser emulation / consumer-web-app clients for third-party services; official APIs only.

## Structure

Target layout (created as the work needs it, not up front):

| Path                  | Purpose                                                  |
| --------------------- | -------------------------------------------------------- |
| `crates/desktop/`     | Tauri app: commands, background threads, state, events   |
| `crates/api_clients/` | One crate per AI provider (official API)                 |
| `crates/lib/`         | Small shared crates (`errors`, files, mimetypes, ...)    |
| `crates/database/`    | SQLite task persistence (sqlx)                           |
| `frontend/`           | React + Vite + Zustand (+ Three.js)                      |
| `_database/`          | SQLite migrations; `.sqlx/` holds offline query metadata |
| `test_data/`          | Test fixtures                                            |
| `packaging/`          | Tester guide (`CTI-ME.txt`), winget manifest draft       |
| `.claude/rules/`      | Detailed agent rules: architecture, code style, testing  |
| `secrets/`            | Never read, edit, or commit                              |

## Working mode: Ponytail (write less code)

The base mindset for this repo comes from [Ponytail](https://github.com/DietrichGebert/ponytail).
When the plugin is installed, its skill is authoritative; this is the short version:

1. **Map before writing.** List every place a change must reach (callers, tests, config, exports) and what it could break.
2. **Pick the smallest complete option**, in order: don't build what wasn't asked → reuse existing code → stdlib/platform → already-installed deps → a readable one-liner → minimal new code.
3. **Lazy solution, full scope.** Every part of the task still gets done.
4. **Bugs:** find all callers, fix the root cause once in shared code.
5. **Moving code:** keep its validation and error handling.
6. **Shortcuts** get a comment stating the known limit and when to upgrade.
7. **Never cut:** trust-boundary validation, data-loss handling, security, accessibility, anything the user asked for.
8. **Report:** end with 1–2 lines on what was skipped, unchecked, or risky.

## Navigating code: CodeGraph

CodeGraph (local devDependency, MCP server in `.mcp.json`) indexes the repo into a symbol graph.
Prefer `codegraph_explore` / `codegraph_node` over broad grep + reading whole files.
After large refactors run `npm run graph:sync`; first time run `npm run graph:init`.

## Dev loop

- `scripts/dev-rebuild.ps1 [-Apo] [-Test] [-Package] [-Start]`: stops the running app, builds it (and with
  `-Apo` also the microphone APO DLL), runs tests and clippy, makes a tester ZIP (`-Package`, see below)
  and starts the app again.
- Claude Code: delegate it to the `dev-rebuild` subagent (`.claude/agents/dev-rebuild.md`, Haiku), so build
  output stays out of the main context.
- Debug capture: `-Apo -DebugCapture` builds the APO with the `debug-capture` feature. After installing it,
  `cargo run --release -p gamersequalizer-apo --example debug_capture -- [seconds]` records up to 60 s of
  every mic-chain stage (WAV) plus per-block variables (`vars.tsv`, `info.txt`) into
  `test_data/private/captures/` (git-ignored). Never ship an APO built with it.
- The APO is installed via the app's banner or `scripts/apo-install.ps1`. Both need admin and restart
  Windows audio, so a human runs them.
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
  `scripts/tests/apo-install.tests.ps1` (part of `-Test`, also runs under `pwsh` on macOS). Only Windows 11
  has been tried on real hardware so far.
- Devices: the setup card installs on the microphone and output in use; other connected devices are
  opt-in checkboxes. A device whose install the installer rolled back is not offered again until "Try
  again". Before changing an endpoint the installer saves its original effect lists once to
  `%ProgramFiles%\GamersEqualizer\apo-backup\`; the uninstaller restores them, then removes our effects.
  It moves the driver's legacy effect values into composite lists only in the slots ours go into (a
  legacy value shadows only its own slot). A device that does not open during the self-test is opened
  once more without our effects: if it opens then, ours stay out; if not (exclusive use, privacy), ours
  go back in unchecked, unless driver effects were moved for them. An endpoint without `FxProperties`
  (inbox "High Definition Audio Device", VMs; Administrators may not create subkeys there) gets the key,
  made with the backup/restore privileges so no owner or ACL changes; its backup is marked `Created`,
  and rollback and uninstall delete the key.
- Mic effect slot: the installer puts the mic chain in EFX, then opens the mic briefly
  (`gamersequalizer.exe --probe-mic {guid}`) and moves it to MFX when Windows did not start it there
  (some notebook mic arrays). The Mic tab offers the same switch later ("Try another effect slot").

## Tester package

ZIP for testers on other PCs (no setup.exe; the app installs/uninstalls itself and its system parts):
`gamersequalizer.exe` + `gamersequalizer_apo.dll` + `CTI-ME.txt` (Czech tester guide, `packaging/CTI-ME.txt`).

- Installed layout: the installer (one admin prompt) copies the exe to `%ProgramFiles%\GamersEqualizer\`
  next to `apo\gamersequalizer_apo.dll`. A copy started from elsewhere then starts the installed one
  (`handover_target` in `apo_install.rs`), and "start with Windows" points there. A newer copy ends
  an older running one first (`takeover.rs`), so an update is "unpack, start, click Update". A dev build
  (`target\<profile>\`) runs in place and is never installed. User data stays in `%APPDATA%\GamersEqualizer`.

- `scripts/package.ps1`: final build from scratch (`dist` + `apo-dist` profiles, fat LTO, minutes; verifies
  the rustc flags in the verbose log). `-Fast` packages the `release`
  build instead (seconds); `scripts/dev-rebuild.ps1 -Package` does that after the dev build.
- Output: `target/package/GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>[-dirty][-fast].zip`.
- Version (`crates/build-support/version_info.rs`): a GitHub build of a tag (`0.1.0` or `v0.1.0`) shows
  that version in the About card, logs and the exe/DLL details; it fails on a tag that is not `X.Y.Z`.
  Every other build shows the short commit hash, `-dirty` when the sources differ from it; `<version>`
  in the ZIP name is then `dev`. The `version` in the Cargo.toml files is not shown anywhere.
  Old ZIPs are never deleted or overwritten; only `target/package/staging` is cleared.
- `apo-install.ps1` / `apo-uninstall.ps1` are built into the exe (`include_str!` in
  `win_audio/apo_install.rs`) and written to `%LOCALAPPDATA%\GamersEqualizer\scripts` before each
  elevated run. The DLL stays a separate file next to the exe (audiodg loads it from disk).
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

## Before a release or a push to main

Before tagging a release or pushing/merging into `main`, offer the user a dependency check. Offer it, don't
run it unasked; updates the user picks go into their own commit.

- **GitHub Actions** (`.github/workflows/*.yml`): every `uses:` is pinned to a full commit SHA plus a
  `# vX.Y.Z` comment, because a tag can be moved to other code (the release job holds the signing
  certificate). Check that each SHA still belongs to that tag upstream
  (`git ls-remote https://github.com/<owner>/<repo>`; GitHub also accepts SHAs from forks) and list newer
  releases. An update takes the new tag's SHA and comment; never switch to a bare tag.
- **Rust crates** (whole workspace, `Cargo.lock`): `cargo update --dry-run --verbose` lists
  semver-compatible updates (`Updating`) and newer major versions (`Unchanged ... (available: ...)`); it
  works on the Mac too. Known vulnerabilities: `cargo audit`, if installed (installing it needs the user's
  okay, see Ground rules). Report what would change, which major bumps need code changes, and any
  advisories. After applying, build, test and clippy (`dev-rebuild` agent).

## Ground rules

- No global installs (`npm -g`, `pip install` outside a venv). Dependencies go into the project; system tools via `winget`, and ask first.
- Don't add a dependency when the stdlib or an existing dep does the job.
- Don't touch `secrets/` or `.env*`. Delete only tracked files unless explicitly told otherwise.
- Markdown tables: pad cells so columns line up in plain text.
- Follow `.claude/rules/*.md` for style, testing and API conventions.
