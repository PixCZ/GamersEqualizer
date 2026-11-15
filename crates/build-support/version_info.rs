//! The app version and the version resource (Explorer > Properties > Details, Task Manager's name
//! column) for the exe and the APO DLL. Not a crate: both build scripts include this file with
//! `#[path]`, so `env!` below reads the including package's Cargo.toml.

/// Set by the release workflow from the pushed tag (`0.1.0` or `v0.1.0`); unset in every other build.
const RELEASE_VERSION_ENV: &str = "GAMERSEQUALIZER_RELEASE_VERSION";
/// What every binary is built from besides its own crate, relative to a crate dir (where build
/// scripts run). Vendor crates by their parts, not whole: `cargo test` of an excluded vendor crate
/// writes a `target/` inside it, which would rerun this script and relink the binaries.
const SHARED_SOURCES: [&str; 14] = [
  "../../Cargo.toml",
  "../../Cargo.lock",
  "../../.cargo",
  "../gamersequalizer-common",
  "../gamersequalizer-mic-chain",
  "../vendor/deep_filter/Cargo.toml",
  "../vendor/deep_filter/src",
  "../vendor/deep_filter/models",
  "../vendor/rnnoise2/Cargo.toml",
  "../vendor/rnnoise2/src",
  "../vendor/rnnoise2/models",
  "../vendor/silero_vad/Cargo.toml",
  "../vendor/silero_vad/src",
  "../vendor/silero_vad/models",
];
/// Windows `VS_FIXEDFILEINFO` values, written as numbers so the .rc needs no `winver.h` include.
const VOS_NT_WINDOWS32: u32 = 0x0004_0004;
const VS_FFI_FILEFLAGSMASK: u32 = 0x3F;
/// US English, Unicode: the language block and `Translation` pair Explorer looks up.
const LANG_BLOCK: &str = "040904B0";
const TRANSLATION: &str = "0x409, 1200";

pub enum FileKind {
  App,
  Dll,
}

/// What differs between the exe and the DLL; everything else comes from Cargo.
pub struct VersionInfo<'a> {
  pub kind: FileKind,
  /// Task Manager shows this as the process name.
  pub description: &'a str,
  /// File name without the extension.
  pub internal_name: &'a str,
}

/// The version every place shows (About card, logs, version resource): the tag's version in a
/// release build, otherwise the short commit hash, with `-dirty` when `sources` (paths relative to
/// the crate dir) or the shared sources differ from that commit. "unknown" outside a git checkout.
pub fn version(sources: &[&str]) -> String {
  // Self-check of the tag parser (a `#[cfg(test)]` cannot run in a file included by build scripts).
  assert_eq!(numeric_version("1.20.3-rc1"), Some([1, 20, 3]));
  assert!(
    numeric_version("1.2").is_none() && numeric_version("1.2.3.4").is_none() && numeric_version("fd9dbf7").is_none()
  );
  println!("cargo:rerun-if-env-changed={RELEASE_VERSION_ENV}");
  if let Some(tag) = std::env::var(RELEASE_VERSION_ENV).ok().filter(|tag| !tag.trim().is_empty()) {
    let version = tag.trim().trim_start_matches('v').to_string();
    // A release must not ship a version Explorer cannot show: fail the build instead.
    #[allow(clippy::expect_used)]
    numeric_version(&version).expect("GAMERSEQUALIZER_RELEASE_VERSION must look like 1.2.3 or v1.2.3");
    return version;
  }
  let watched: Vec<&str> = sources.iter().chain(&SHARED_SOURCES).copied().collect();
  // A changed file reruns this script, so `-dirty` appears and goes away with the edits.
  for path in &watched {
    println!("cargo:rerun-if-changed={path}");
  }
  let Some(hash) = head_hash() else { return "unknown".into() };
  // No optional locks: a build must not race the user's own git commands for the index.
  let mut status = vec!["--no-optional-locks", "status", "--porcelain", "--untracked-files=no", "--"];
  status.extend(&watched);
  // shortcut: a failing `git status` reads as clean; it only fails on a broken checkout.
  if git(&status).is_some() { format!("{hash}-dirty") } else { hash }
}

/// Short commit hash. Also tells cargo to rerun the build script when HEAD moves (commit,
/// checkout, reset), including in a worktree.
fn head_hash() -> Option<String> {
  // The reflog changes on every move of HEAD. Watching a missing path would rerun the script
  // (and relink) on every build, so without a reflog only `HEAD` itself is watched.
  // shortcut: then a commit on the same branch keeps the old hash until a checkout; enable the
  // reflog (`core.logAllRefUpdates`) if that matters.
  let watched = git(&["rev-parse", "--git-path", "logs/HEAD"])
    .filter(|path| std::path::Path::new(path).exists())
    .or_else(|| git(&["rev-parse", "--git-path", "HEAD"]));
  if let Some(path) = watched {
    println!("cargo:rerun-if-changed={path}");
  }
  git(&["rev-parse", "--short", "HEAD"])
}

impl VersionInfo<'_> {
  /// A complete UTF-8 .rc file with one `VERSIONINFO` resource; more resources may be appended.
  /// `version` comes from [`version`]; a commit hash has no numbers, so its file version is 0.0.0.0.
  pub fn rc(&self, version: &str) -> String {
    let [major, minor, patch] = numeric_version(version).unwrap_or_default();
    let authors = env!("CARGO_PKG_AUTHORS").replace(':', ", ");
    let (file_type, extension) = match self.kind {
      FileKind::App => (1, "exe"),
      FileKind::Dll => (2, "dll"),
    };
    let strings = [
      ("CompanyName", authors.clone()),
      ("FileDescription", self.description.to_string()),
      ("FileVersion", version.to_string()),
      ("InternalName", self.internal_name.to_string()),
      ("LegalCopyright", format!("Copyright (C) {authors}")),
      ("OriginalFilename", format!("{}.{extension}", self.internal_name)),
      ("ProductName", "GamersEqualizer".to_string()),
      ("ProductVersion", version.to_string()),
    ];
    let values: String =
      strings.iter().map(|(name, value)| format!("      VALUE \"{name}\", {}\n", rc_wide_string(value))).collect();
    format!(
      r#"1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK {VS_FFI_FILEFLAGSMASK:#x}
FILEFLAGS 0x0
FILEOS {VOS_NT_WINDOWS32:#x}
FILETYPE {file_type:#x}
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "{LANG_BLOCK}"
    BEGIN
{values}    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", {TRANSLATION}
  END
END
"#
    )
  }
}

/// `1.2.3` from `1.2.3`, `1.2.3-rc1` or `1.2.3+build`; None for anything else (a commit hash).
fn numeric_version(version: &str) -> Option<[u16; 3]> {
  let core = version.split(['-', '+']).next()?;
  let mut parts = core.split('.').map(|part| part.parse::<u16>().ok());
  let numbers = [parts.next()??, parts.next()??, parts.next()??];
  parts.next().is_none().then_some(numbers)
}

/// Trimmed stdout of `git <args>`; None when git is missing or fails.
fn git(args: &[&str]) -> Option<String> {
  let out = std::process::Command::new("git").args(args).output().ok()?;
  let text = String::from_utf8(out.stdout).ok()?;
  let text = text.trim();
  (out.status.success() && !text.is_empty()).then(|| text.to_string())
}

/// Wide .rc string literal in plain ASCII: other characters become `\xHHHH` UTF-16 units, so the
/// result does not depend on the resource compiler's code page (the author's name has háčky;
/// llvm-rc ignores `#pragma code_page`). Values come from Cargo.toml and git, never the user.
fn rc_wide_string(s: &str) -> String {
  let mut out = String::from("L\"");
  let mut after_escape = false;
  for c in s.chars() {
    let plain = c.is_ascii_graphic() || c == ' ';
    // A hex digit right after `\xHHHH` could be read as part of that escape: escape it too.
    if plain && !(after_escape && c.is_ascii_hexdigit()) {
      match c {
        '"' => out.push_str("\"\""),
        '\\' => out.push_str("\\\\"),
        _ => out.push(c),
      }
      after_escape = false;
    } else {
      let mut units = [0u16; 2];
      for unit in c.encode_utf16(&mut units) {
        out.push_str(&format!("\\x{unit:04x}"));
      }
      after_escape = true;
    }
  }
  out.push('"');
  out
}
