# Architecture: audio never goes down

Goal: after startup, no user action, device change, bad file or mismatched build can crash the app,
and nothing we run inside `audiodg.exe` can take Windows audio down. Errors are values: they get
logged, shown, retried, never unwound.

## Where the code runs

| Process                        | What runs there                                         | When it fails                               |
| ------------------------------ | ------------------------------------------------------- | ------------------------------------------- |
| `audiodg.exe` (Windows)        | `gamersequalizer_apo.dll` on the audio engine's threads | `catch_unwind` -> audio passes through      |
| `gamersequalizer.exe --daemon` | Service: 50 ms main loop plus worker threads            | `panic = "abort"`: a panic ends the service |
| `gamersequalizer.exe`          | The egui window; closing it leaves the service running  | `panic = "abort"`: a panic ends the window  |

- The APO is built with `panic = "unwind"` (profiles `apo`, `apo-dist`) and wraps every call into the
  DSP chain and every processor build / drop in `catch_unwind`. The COM entry points that build
  something (`DllGetClassObject`, `CreateInstance`, Initialize, LockForProcess) run in `guarded`:
  a panic fails that call with `E_UNEXPECTED` and leaves an `[ApoNote]`. The other entry points
  only check pointers and copy values. The exe aborts on panic, so there the only protection is
  not panicking.
- How the processes share state:
  - Shared memory (`gamersequalizer-common/src/shm/`): a `Local\` mapping between window and service,
    and a `Global\` mapping the APO creates inside audiodg. The service mirrors settings into it and
    telemetry out of it (`win_audio/apo_link.rs`); the window opens it too.
  - A second `Local\` mapping (`mixer_edits.rs`) tells the window what the service changed in the
    mixer (the tray flyout's channel levels and output pick). It is outside the versioned layout.
  - JSON files in `%APPDATA%\GamersEqualizer`, written by both the window and the service.
  - The APO's notes in the Windows Application log, which the service copies into the app log as
    `[ApoNote]` (`health/apo_diag.rs`).
- One process is the owner: it publishes settings and bumps `owner_beat`. When the beat stops for
  `OWNER_TIMEOUT`, the APO passes audio through untouched.

## Two phases

| Phase     | Allowed to panic? | Rule                                                                  |
| --------- | ----------------- | --------------------------------------------------------------------- |
| Bootstrap | Yes, fail fast    | Before the service loop or the window starts. Log, then `expect`.     |
| Running   | Never             | Loops, threads, UI callbacks and state managers return `Result`.      |

## Rules for the running phase

- No `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, or slice indexing `v[i]` on untrusted data.
  Enforced by lints (see `code-style.md`). An exception is `#[expect(lint, reason = "...")]`, the
  reason proving it cannot fail.
- **Errors**: `Result<T, String>` where the caller only logs or shows the text. Where a caller decides
  by the kind of failure, return an enum. Never compare error text.
- **Background work = supervised loop.** A worker thread is an outer `loop` around one fallible
  iteration. It never returns on a failure that can pass (device unplugged, audio service restarting,
  shared memory not there yet):
  ```rust
  loop {
    if let Err(err) = run_once(&ctx) {
      log_changed(&mut last_error, &err); // the same error again is not logged again
      thread::sleep(backoff.next());      // capped, never a hot loop
      continue;
    }
    backoff.reset();
  }
  ```
- **State files load infallibly.** Missing or corrupt file: log, keep the corrupt copy
  (`keep_corrupt`), use defaults, keep running. Writes are atomic (`write_atomic`,
  `output_eq/settings.rs`). Never block startup on a device or another process.
- **Stored data is forward-compatible.** Enums in stored files have a catch-all (`#[serde(other)]`, or
  `Unknown(String)` when the value must survive a save), unknown fields survive a load-save round
  trip, and an older app meeting a newer file neither crashes nor destroys it.
- **Never lose the user's work silently**: a profile is replaced only by an action that says so; what
  is replaced otherwise goes to the deleted folder (`move_to_deleted`).
- **Locks**: never hold one across a call into Windows audio that can block. A poisoned lock is not a
  reason to panic.
- **Diagnostics**: the app log alone must explain a failure (see `installer.md`).

## Real-time audio threads

Applies to the APO's `rt_*` path (including the output APO's reference feed), the mic chain's
`process*` functions and the WASAPI loop in the service (sidetone).

- Per block: no allocation, no blocking lock (`try_lock` only), no file I/O, no logging. A failure
  that repeats is logged once (a flag), not per frame.
- Settings come from shared-memory atomics. Rebuild derived state (EQ coefficients, model choice) when
  a version counter changes, not every block.
- The mic chain runs at 48 kHz mono. Other device rates go through `RateBridge` in the APO.
- Slow work (loading a model) runs on a helper thread and is handed over when ready; until then audio
  passes with the previous or no processing.
- State that depends on past audio is cleared on every stream restart and mode switch, in one place.
- Known exception: tract (DeepFilterNet 3, DPDFNet) allocates inside inference.
- `debug-capture` code is feature-gated and never ships.

## Shared-memory layout

- Two processes that may be different builds read it (an old DLL meets a new exe after an update).
  Every layout change bumps `shm_version!` in `shm/mod.rs`; `SHM_VERSION` and both mapping names follow it,
  and a size check fails the build until it is bumped. A mapping of another version counts as missing.
- Fields are atomics. A value made of several fields is published with a version counter that the
  reader checks after reading.
- A new field goes into the struct, `init_defaults` and the right `copy_*` mirror list.

## Unsafe and COM

- Win32, COM and APO FFI are everywhere, so `unsafe` is allowed. Every `unsafe` block carries a
  `// SAFETY:` comment that says why it holds (where the pointer comes from, how long it lives,
  which check makes the access valid), enforced by the `undocumented_unsafe_blocks` lint. An
  `unsafe fn` states its contract under `# Safety`. A run of COM calls on one object is one block.
- A thread that uses COM initializes it itself. Strings Windows allocates (`GetId`,
  `PropVariantToStringAlloc`) are freed with `CoTaskMemFree` on every path.

## Elevation

- Only `apo-install.ps1` / `apo-uninstall.ps1` run as administrator, plus the app's `--probe-mic` /
  `--probe-render` mode, which the install script starts elevated from its own copy of the exe. The
  scripts are built into the exe, written to disk, and the elevated bootstrap checks their SHA-256
  before running them (`win_audio/apo_install/elevated.rs`).
- Elevated code never reads or loads anything a non-admin process can still change after that check.
- No second route to admin rights.

## Not in this app

- **No network code**: no HTTP or socket crates, no updater, no telemetry. A link opens in the user's
  browser through the shell.
- No WebView, Tauri or Electron; the UI is egui.
- No virtual audio devices and no kernel drivers.
