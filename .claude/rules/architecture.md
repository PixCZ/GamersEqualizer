# Architecture: the app never goes down

Goal: once the window is up, no user action, network failure, bad file or provider outage
can crash the Rust process. Errors are values; they get logged, shown, retried — never unwound.
Pattern taken from ArtCraft's desktop crate, with its gaps closed.

## Two phases

| Phase      | Allowed to panic? | Rule                                                                 |
| ---------- | ----------------- | -------------------------------------------------------------------- |
| Bootstrap  | Yes, fail fast    | Before `tauri::Builder::run`: data dir, config. Log, then `expect`.  |
| Running    | Never             | Commands, threads, event handlers, state managers return `Result`.   |

## Rules for the running phase

- No `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, slice indexing `v[i]` on untrusted data.
  Enforced by lints (see `code-style.md`). `#[allow]` needs a comment proving it cannot fail.
- **Commands**: every `#[tauri::command]` returns `Result<SuccessResponse<T>, FailureResponse>`.
  Failures carry `error_code` (stable, machine-readable) + `message`. The frontend handles every failure.
- **One app error enum** (`AppError`) with `From` impls for io/http/db/provider errors so `?` just works.
  `Display` is for logs; what the user sees is mapped from `error_code`, never raw `{:?}`.
- **Background work = supervised loop.** Each worker is an outer `loop` around one fallible iteration:
  ```rust
  loop {
    if let Err(err) = poll_iteration(&ctx).await {
      error!("[Worker] iteration failed: {:?}", err);
      tokio::time::sleep(SLEEP_ON_ERROR).await;
      continue;
    }
    tokio::time::sleep(next_sleep(&ctx)).await;
  }
  ```
  Always log the error (ArtCraft drops it in places — don't). One failing job must not stop the others:
  handle per job, mark it failed in the DB, move on. Retry with capped backoff, not a hot loop.
- **Durable jobs**: long-running work (generation, uploads, downloads) is a row in the SQLite task table
  before it starts. On restart, workers resume `pending` rows. The UI reads status from the DB/events,
  not from in-memory state that a restart would lose.
- **State managers load infallibly**: `load_or_default(path)`. Missing or corrupt file → log, use defaults,
  keep running. Never block startup on a provider or network.
- **Locks**: never hold a `std` lock across `.await` (`await_holding_lock` is denied). Lock poisoning maps
  to an `AppError` variant, not `unwrap`. Prefer message passing / `tokio::sync` for shared async state.
- **Frontend contract is forward-compatible**: enums crossing the boundary (task status, provider, media type)
  have an `Unknown(String)` catch-all; unknown JSON fields are ignored. Old app + new data never crashes.
- **Events, not polling from JS**: backend workers emit Tauri events on state changes; the frontend subscribes.

## Providers (external AI services)

- One client crate per provider, behind a common trait the app talks to. Provider errors become `AppError`.
- **Official APIs only.** No browser emulation, cookie scraping, consumer-web-app clients or
  anti-bot/Cloudflare workarounds (ArtCraft has these; we deliberately don't).
- Credentials: OS keychain or the app data dir, never in the repo, never logged.

## Layout (target)

```
crates/
  desktop/app/        Tauri app: core/{commands,threads,state,events,lifecycle}, app_error.rs
  api_clients/<name>/ one crate per provider (official API)
  lib/errors/         pins anyhow, exports AnyhowResult/AnyhowError
  lib/...             small shared crates (files, mimetypes, urls)
  database/tasks/     SQLite task persistence (sqlx), migrations in _database/
frontend/             React + Vite + Zustand (+ Three.js), talks only via commands/events
```
