# Code Style

## General

- Formatting is the formatter's job (`rustfmt`, Prettier); don't hand-format or argue with it.
- Two-space indentation in Rust and TypeScript.
- Prefer imports over inline fully-qualified paths; alias name collisions with a suffix (`TaskDb`, `TaskApi`).

## Rust

- Logging only via `log` macros (`error!`, `warn!`, `info!`, `debug!`, `trace!`). No `println!`/`eprintln!`
  outside tests and pre-logger bootstrap.
- Application code: `AnyhowResult<T>` from the `errors` crate (never depend on `anyhow` directly).
  Typed errors at boundaries: `AppError` for commands, `{Feature}Error` enums inside crates (`thiserror`).
- DB queries: SQLx `query!` / `query_as!` macros, return `Result<T, sqlx::Error>`.
- Tauri: one command per file, named after it (`generate_image_command.rs`); shared code in a support module.
  All commands registered in one `generate_handler!` list.
- No panics after bootstrap — see `architecture.md`.

### Lints (`[workspace.lints]` in root `Cargo.toml`)

```toml
[workspace.lints.rust]
unsafe_code = "deny"
unused_must_use = "deny"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
unimplemented = "deny"
indexing_slicing = "warn"
await_holding_lock = "deny"
let_underscore_future = "deny"
clone_on_copy = "deny"
needless_return = "deny"
redundant_field_names = "deny"
```

Bootstrap code and tests opt out locally with `#[allow(clippy::expect_used)]` + a reason comment.

## TypeScript / Frontend

- React + Vite + Zustand; Three.js for 3D. Strict TS, no `any` at the command boundary.
- Every `invoke()` goes through one typed wrapper that returns success | failure; no bare try/catch per call site.
- A React error boundary around each major panel; one broken panel doesn't blank the app.

## File Layout

Read top to bottom:

1. Constants
2. Types (structs/enums/interfaces), then API types: Request, Response, Error
3. Constructors, then public functions/methods, then private helpers
4. Tests last
