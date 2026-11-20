# Building GamersEqualizer

## Full optimized build (fat LTO)

For a build that goes to testers. Needs the Rust MSVC toolchain on Windows; takes about 7–8 minutes.

1. Commit everything first. Uncommitted changes add `-dirty` to the ZIP name.
2. From the repo root in PowerShell:

   ```powershell
   .\scripts\package.ps1
   ```

   The script always builds from scratch (`cargo clean --profile dist` / `--profile apo-dist`, never
   the dev build or older ZIPs), ignores environment overrides such as `RUSTFLAGS` or `CARGO_PROFILE_*`,
   builds with `--locked`, and then checks the real rustc command lines in the verbose log: it fails
   unless both binaries were linked with the flags below and without the `debug-capture` and `cpu-stats` features.

3. The result is `target\package\GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>.zip`; the script prints
   its path, size and SHA256. Next to it: `<name>.build.log` (full cargo output) and `<name>.build.txt`
   (toolchain, commit, flags, SHA256 of each file). Older ZIPs are kept.

What the script runs (both: `--locked -v`, static CRT, stripped):

| Step                                                    | Profile    | Optimization                                                        |
| ------------------------------------------------------- | ---------- | ------------------------------------------------------------------- |
| `cargo build --profile dist -p gamersequalizer`         | `dist`     | `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, no incremental |
| `cargo build --profile apo-dist -p gamersequalizer-apo` | `apo-dist` | same as `dist`, but `panic = "unwind"`                              |

The APO DLL keeps `panic = "unwind"` so a DSP panic inside `audiodg.exe` turns into pass-through audio
instead of killing system audio. Code is generic x86-64 (no `target-cpu=native`), so it runs on any tester PC.

The app exe alone, without packaging:

```powershell
cargo build --profile dist -p gamersequalizer   # -> target\dist\gamersequalizer.exe
```

Quick package from the dev build (no LTO, seconds): `.\scripts\package.ps1 -Fast` or
`.\scripts\dev-rebuild.ps1 -Package`. Never package an APO built with `-DebugCapture`.
