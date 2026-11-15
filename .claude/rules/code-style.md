# Code Style

## General

- Formatting is `rustfmt`'s job (`rustfmt.toml`: two-space indentation, 120 columns); don't hand-format.
- Prefer imports over inline fully-qualified paths; alias name collisions with a suffix.
- Code, comments, commit messages and docs are in English. A comment says why, not what.
- Names say what the thing is today. When a design is replaced, its names, fields and comments go too.

## Rust

- Logging only via `log` macros (`error!`, `warn!`, `info!`, `debug!`, `trace!`), with the component
  in brackets first: `info!("[Loopback] ...")`. No `println!` / `eprintln!` outside tests, examples
  and pre-logger bootstrap.
- Errors: see `architecture.md` (`Result<T, String>` for log-and-show, an enum where the caller
  decides by kind).
- A new dependency needs a reason the standard library and the existing dependencies cannot cover;
  the reason goes into a comment in `Cargo.toml`, as the existing ones have.
- No panics after bootstrap, `unsafe` only with a `// SAFETY:` comment: see `architecture.md`.

### Lints (`[workspace.lints]` in the root `Cargo.toml`)

```toml
[workspace.lints.rust]
unused_must_use = "deny"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
unimplemented = "deny"
indexing_slicing = "deny"
await_holding_lock = "deny"
let_underscore_future = "deny"
clone_on_copy = "deny"
needless_return = "deny"
redundant_field_names = "deny"
undocumented_unsafe_blocks = "deny"
```

- Every crate inherits them (`[lints] workspace = true`); `crates/vendor/` is outside the workspace.
- Tests may unwrap, expect, panic and index (`clippy.toml`).
- Bootstrap code opts out locally with `#[expect(clippy::expect_used, reason = "...")]`.
- An exception is `#[expect(..., reason = "...")]`, so the compiler reports it once it is not needed.
  `#[allow]` only where the lint fires in some configurations (`cfg`), with a comment saying which.

## UI (egui)

- Every text the user sees goes through `tr("English text")`, `trf` (with arguments) or `tk` (in a
  constant). The English text is the key; add the Czech entry to `i18n_cs.rs` in the same change. A
  test (`i18n.rs`) reads every file under `src/` and fails on a missing one. It sees only literals
  passed straight to `tr` / `trf` / `tk`, so a text in a constant goes through `tk`.
- New features reuse the shared widgets: the EQ band editor (`ui/eq_editor.rs`, `ui/eq_graph.rs`) and
  volumes in percent with a defined 100 %. No parallel widgets, no bare dB gains.
- The window repaints many times a second: no file, registry or Windows audio calls in the paint path.
  Read on a timer or on an event and keep the result in the state.
- Say what happens and why, in place (label, tooltip, status line); state a limit honestly with the
  next step.

## PowerShell (`scripts/`)

- `apo-install.ps1` / `apo-uninstall.ps1` run elevated under Windows PowerShell 5.1; their self-check
  (`scripts/tests/apo-install.tests.ps1`) also runs under `pwsh` on macOS. Code must work in both.
- Constants shared with Rust (CLSIDs, registry value names, device-state bits) must agree with
  `win_audio/apo_install.rs` (test `scripts_use_the_same_constants`).

## File Layout

Read top to bottom:

1. Constants
2. Types (structs / enums)
3. Constructors, then public functions / methods, then private helpers
4. Tests last

A file holds one job. Split by job, never by line count.
