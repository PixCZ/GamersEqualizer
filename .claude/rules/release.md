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
- **tract version and our fork** (pinned `=0.23.10`; `[patch.crates-io]` in the root `Cargo.toml` takes
  `tract-linalg` / `tract-data` from github.com/PixCZ/tract, branch `gamersequalizer-0.23.10`, pinned
  by commit). The fork carries an AVX-512 kernel for f16 weights, a kernel choice fix for small
  matrix-vector products and a 16x1 kernel fix, all merged into sonos/tract main on 2026-11-20
  (sonos/tract#3487, #3488, #3489), after 0.23.10 was released. Every time, look up the newest tract
  version (`cargo search tract-linalg --limit 1`) and report it beside the pinned one. A newer
  version carries the three changes when
  `gh api repos/sonos/tract/compare/<its tag>...31a0dc6d2 -q .status` answers `behind` or `identical`
  (`ahead` or `diverged`: it does not, or it took them as other commits, so read its changelog).
  Once a release carries them, move every tract crate to it, drop the patch and delete the fork
  branch. A newer release without them means rebasing the fork branch onto it (the patch must match
  the exact `=0.23.x` pin).
- **egui-wgpu fork** (`crates/vendor/egui-wgpu`, `[patch.crates-io]` in the root `Cargo.toml`):
  0.37.0 plus emilk/egui#8784, so the window does not keep the NVIDIA GPU of hybrid laptops awake
  (issue emilk/egui#8704). Report the state of #8784 and whether an egui release carries it; once one
  does, move eframe / egui / egui-wgpu to that release, delete the directory and its two lines in the
  root `Cargo.toml` (`[patch.crates-io]`, `exclude`). An egui bump before that has to re-vendor the
  matching egui-wgpu version with the same change, or the patch no longer applies.
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
