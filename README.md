# GamersEqualizer

Windows desktop app: output EQ, chat mix and a microphone chain (system APO).
Development notes live in `AGENTS.md`, the history in `DEVLOG.md`.

## Full optimized build (fat LTO)

For a build that goes to testers. Needs the Rust MSVC toolchain on Windows; takes about 7–8 minutes.

1. Commit everything first. Uncommitted changes add `-dirty` to the ZIP name.
2. From the repo root in PowerShell:

   ```powershell
   .\scripts\package.ps1
   ```

   Cargo reuses an up-to-date build: if nothing changed since the last `dist` build, this takes seconds
   and packages that same (fully optimized) exe. To force a build from scratch, clean both profiles first:

   ```powershell
   cargo clean --profile dist
   cargo clean --profile apo
   ```

   (`cargo clean` alone wipes all of `target`, including the dev build and the ZIPs in `target\package`.)

3. The result is `target\package\GamersEqualizer-<version>-<yyyyMMdd-HHmmss>-<commit>.zip`; the script prints
   its path, size and SHA256. Older ZIPs are kept.

What the script runs:

| Step                                               | Profile | Optimization                                                 |
| -------------------------------------------------- | ------- | ------------------------------------------------------------ |
| `cargo build --profile dist -p gamersequalizer`    | `dist`  | `lto = "fat"`, `codegen-units = 1`, no incremental           |
| `cargo build --profile apo -p gamersequalizer-apo` | `apo`   | like `release` (`opt-level = 3`, no LTO), `panic = "unwind"` |

Only the app exe gets fat LTO. The APO DLL uses the `apo` profile, which inherits `release`, so it is
optimized (`opt-level = 3`) but not LTO-linked. It keeps `panic = "unwind"` so a DSP panic inside
`audiodg.exe` turns into pass-through audio instead of killing system audio.

The app exe alone, without packaging:

```powershell
cargo build --profile dist -p gamersequalizer   # -> target\dist\gamersequalizer.exe
```

Quick package from the dev build (no LTO, seconds): `.\scripts\package.ps1 -Fast` or
`.\scripts\dev-rebuild.ps1 -Package`. Never package an APO built with `-DebugCapture`.
