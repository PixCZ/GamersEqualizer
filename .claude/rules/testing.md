# Testing

## Running

- Rust: `cargo test` (workspace), `cargo clippy --workspace --all-targets` must be clean.
  Use `SQLX_OFFLINE=true` when no dev DB is available (keep `.sqlx/` metadata committed).
- For a quick compile check prefer `cargo check` over a full build.
- Frontend: the package's `test` script (Vitest).

## What to Test

- Non-trivial logic: parsers and their edge cases, error mapping to `error_code`, serialization of
  stored/cross-boundary enums (round-trip + `Unknown` fallback), task state transitions.
- Worker iterations: a failing job is marked failed and the next job still runs.
- Bug fixes: one regression test that fails before the fix.

## What Not to Test

- Trivial constructors, getters, pass-through wrappers.
- Real external provider APIs. Stub them in-process (local HTTP server / fake trait impl).

## Layout

- Rust: `#[cfg(test)] mod tests` at the bottom of the file; imports, constants, tests, helpers.
- Shared fixtures in one `test_utils` crate / `test_data/` dir, not copy-pasted.
- Tests use temp dirs and in-memory/temp SQLite, never the real app data dir.
