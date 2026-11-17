---
paths:
  - "packaging/release-notes/**"
  - ".github/workflows/**"
  - "Cargo.lock"
  - "rust-toolchain.toml"
---

# Release: dependency check and release notes

## Dependency check (offer before a release tag or a push/merge to main)

Offer it, don't run it unasked; updates the user picks go into their own commit. Claude Code: run the
commands through the `cmd-runner` agent.

- **GitHub Actions** (`.github/workflows/*.yml`): every `uses:` is pinned to a full commit SHA plus a
  `# vX.Y.Z` comment, because a tag can be moved to other code (the release job holds the signing
  certificate). Check that each SHA still belongs to that tag upstream
  (`git ls-remote https://github.com/<owner>/<repo>`; GitHub also accepts SHAs from forks) and list newer
  releases, and flag any action whose version still runs on a deprecated Node (`runs.using` in its
  `action.yml`; GitHub warns about it in the run). An update takes the new tag's SHA and comment; never
  switch to a bare tag.
- **Rust crates** (whole workspace, `Cargo.lock`): `cargo update --dry-run --verbose` lists
  semver-compatible updates (`Updating`) and newer major versions (`Unchanged ... (available: ...)`); it
  works on the Mac too. Known vulnerabilities: `cargo audit` (installed on the dev PC; the `Audit`
  workflow runs it weekly and on every lockfile change, also over the vendor lockfiles). Report what
  would change, which major bumps need code changes, and any advisories. After applying, build, test and
  clippy (`dev-rebuild` agent).
- **tract from our fork** (`[patch.crates-io]` in the root `Cargo.toml`: `tract-linalg` / `tract-data`
  from github.com/PixCZ/tract, branch `gamersequalizer-0.23.10`, pinned by commit): it carries an
  AVX-512 kernel for f16 weights, a kernel choice fix for small matrix-vector products and a 16x1
  kernel fix. Ready for sonos/tract as branches `avx512-f16-matvec` and `avx512-16x1-partials` of
  the fork; the pull requests are not opened yet. Report their state and whether a tract release
  carries them; once one does, move to that release and drop the patch. Moving tract means rebasing
  the fork branch onto the new version (the patch must match the exact `=0.23.x` pin).
- **Rust toolchain** (`rust-toolchain.toml`, used by the dev PC and both workflows): compare the pinned
  version with the current stable (`rustup check`). A bump goes into its own commit and passes the
  gate (format and clippy can change with it).

## Release notes

`packaging/release-notes/<tag>.md` (exactly the tag's name, e.g. `0.2.0.md`) goes above the generated
notes of the GitHub Release (`release.yml` reads it from the tagged commit, so it must be committed
before the tag). Draft it as `next.md` while working and rename it before tagging. Written for gamers
who use the app, not for developers; `packaging/release-notes/0.2.0.md` is the model:

- **One theme with a name** as the heading (`## Rock solid`), then a short paragraph saying what the
  release means for the user as a whole. Not a list of changes.
- **Three to five bullets** that carry the theme, each a bold benefit ("Your sound survives a failed
  update.") plus one plain sentence on what the user notices.
- **Small fixes in one "Also:" sentence.** Leave out what the user cannot notice: refactoring,
  logging, tests, CI, forward compatibility, installer internals.
- Then `### Removed` (only when something went away, with what to do instead) and `### Updating`.
- Short: it fits on one screen. No internal names (APO, shared memory, AEC3); say what it does
  ("the echo canceller", "the microphone effect").
