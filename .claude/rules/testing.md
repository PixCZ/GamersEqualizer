# Testing

## Running

- The workspace builds only on Windows (the `windows` crate, the APO). The gate is `scripts/check.ps1`:
  `cargo fmt -- --check`, clippy with `-D warnings` (workspace, and the APO with `debug-capture`),
  `cargo test --workspace`, the installer self-check and the vendor crates' tests. CI runs it on every
  push; `scripts/dev-rebuild.ps1 -Test` runs it after the build. Claude Code: delegate that to the
  `dev-rebuild` subagent. A step is done only when the gate passes.
- For a quick compile check prefer `cargo check` over a full build.
- The vendor forks are outside the workspace and have their own manifests:
  `cargo test --release --manifest-path crates/vendor/<name>/Cargo.toml`.
- Off Windows only the installer self-check runs: `pwsh scripts/tests/apo-install.tests.ps1`.

## What to Test

- Non-trivial logic: parsers and their edge cases (AutoEQ text, share text, Sonar database, Steam and
  launcher files), game and genre matching, health rules.
- Stored and cross-version data: round trip, an unknown enum value, a file from a newer version.
- Shared memory: defaults, the mirror copies, version handling.
- DSP stages against the fixtures in `test_data/` (levels, delay, state after a restart or mode switch).
- Installer logic in `scripts/tests/apo-install.tests.ps1`, including rollback.
- Bug fixes: one regression test that fails before the fix.

## What Not to Test

- Trivial constructors, getters, pass-through wrappers.
- Real audio devices, the registry or the audio service in unit tests. The probe tools in
  `crates/gamersequalizer-apo/examples/` are for that, run by a human.
- A test that asserts nothing. A measurement that only prints is an `#[ignore]`d report, named as one.

## Layout

- `#[cfg(test)] mod tests` at the bottom of the file; imports, constants, tests, helpers.
- Fixtures live in `test_data/`, not copy-pasted. Reports over the developer's recordings read
  `test_data/private/` (git-ignored) and are `#[ignore]`d, so the suite passes without them.
- Tests use temp dirs, never the real `%APPDATA%\GamersEqualizer`.
