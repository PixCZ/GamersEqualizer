//! Embeds the DLL's version resource (Explorer > Properties > Details).

// Shared with the daemon's build script; each uses one `FileKind`.
#[path = "../build-support/version_info.rs"]
#[allow(dead_code)]
mod version_info;

use std::path::PathBuf;
use version_info::{FileKind, VersionInfo};

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=../build-support/version_info.rs");

  let hash = version_info::git_hash();
  // Every Application-log note names the build audiodg really loaded.
  println!("cargo:rustc-env=GAMERSEQUALIZER_GIT_HASH={hash}");
  let info = VersionInfo {
    kind: FileKind::Dll,
    description: "GamersEqualizer audio effects",
    internal_name: "gamersequalizer_apo",
  };
  // Bootstrap tooling, not the running app: failing the build loudly is the right outcome.
  #[allow(clippy::expect_used)]
  let rc_path = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("apo.rc");
  #[allow(clippy::expect_used)]
  std::fs::write(&rc_path, info.rc(&hash)).expect("write apo.rc");
  // Only the cdylib: the probe examples are separate exes without it.
  embed_resource::compile_for_cdylib(&rc_path, embed_resource::NONE).manifest_required().unwrap_or_else(|e| {
    // No resource compiler (no Windows SDK): build without the version rather than fail.
    println!("cargo:warning=DLL version not embedded: {e}");
  });
}
