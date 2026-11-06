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

- `scripts/dev-rebuild.ps1 [-Apo] [-Test] [-Start]`: stops the running app, builds it (and with `-Apo` also
  the microphone APO DLL), runs tests and clippy, and starts the app again.
- Claude Code: delegate it to the `dev-rebuild` subagent (`.claude/agents/dev-rebuild.md`, Haiku), so build
  output stays out of the main context.
- The APO is installed via the app's banner or `scripts/apo-install.ps1`. Both need admin and restart
  Windows audio, so a human runs them.

## Ground rules

- No global installs (`npm -g`, `pip install` outside a venv). Dependencies go into the project; system tools via `winget`, and ask first.
- Don't add a dependency when the stdlib or an existing dep does the job.
- Don't touch `secrets/` or `.env*`. Delete only tracked files unless explicitly told otherwise.
- Markdown tables: pad cells so columns line up in plain text.
- Follow `.claude/rules/*.md` for style, testing and API conventions.
