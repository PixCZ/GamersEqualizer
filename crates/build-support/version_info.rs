//! Version resource (Explorer > Properties > Details, Task Manager's name column) for the exe and
//! the APO DLL, plus the short commit hash. Not a crate: both build scripts include this file with
//! `#[path]`, so `env!` below reads the including package's Cargo.toml.

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

/// Short commit hash, "unknown" outside a git checkout (e.g. a source ZIP). Also tells cargo to
/// rerun the build script when HEAD moves (commit, checkout, reset), including in a worktree.
pub fn git_hash() -> String {
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
  git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

impl VersionInfo<'_> {
  /// A complete UTF-8 .rc file with one `VERSIONINFO` resource; more resources may be appended.
  pub fn rc(&self, git_hash: &str) -> String {
    let (major, minor, patch) =
      (env!("CARGO_PKG_VERSION_MAJOR"), env!("CARGO_PKG_VERSION_MINOR"), env!("CARGO_PKG_VERSION_PATCH"));
    let version = env!("CARGO_PKG_VERSION");
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
      ("ProductVersion", format!("{version} ({git_hash})")),
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
