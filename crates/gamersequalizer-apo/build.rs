//! Embeds the DLL's version resource (Explorer > Properties > Details).

#[path = "../build-support/version_info.rs"]
#[expect(dead_code, reason = "shared with the daemon's build script; each uses one `FileKind`")]
mod version_info;

use std::path::PathBuf;
use version_info::{FileKind, VersionInfo};

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=../build-support/version_info.rs");

  // Every Application-log note names the build audiodg really loaded.
  let version = version_info::version(&["."]);
  println!("cargo:rustc-env=GAMERSEQUALIZER_VERSION={version}");
  let info = VersionInfo {
    kind: FileKind::Dll,
    description: "GamersEqualizer audio effects",
    internal_name: "gamersequalizer_apo",
  };
  #[expect(clippy::expect_used, reason = "build script: a loud failure is the right outcome")]
  let rc_path = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("apo.rc");
  #[expect(clippy::expect_used, reason = "build script: a loud failure is the right outcome")]
  std::fs::write(&rc_path, info.rc(&version)).expect("write apo.rc");
  // Only the cdylib: the probe examples are separate exes without it.
  embed_resource::compile_for_cdylib(&rc_path, embed_resource::NONE).manifest_required().unwrap_or_else(|e| {
    // No resource compiler (no Windows SDK): build without the version rather than fail.
    println!("cargo:warning=DLL version not embedded: {e}");
  });
}
