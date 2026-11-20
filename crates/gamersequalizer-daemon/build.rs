//! Embeds the app icon into the .exe (Explorer, shortcuts, Alt+Tab before the window sets its own)
//! and its version resource. The .ico is generated from `src/app_icon.rs`, the same drawing the
//! window and tray use.

#[path = "src/app_icon.rs"]
mod app_icon;
#[path = "../build-support/version_info.rs"]
#[expect(dead_code, reason = "shared with the APO's build script; each uses one `FileKind`")]
mod version_info;

use std::path::PathBuf;
use version_info::{FileKind, VersionInfo};

/// Sizes Windows picks from: small icons, title/taskbar, Explorer "large" and "extra large" views.
const ICO_SIZES: [u32; 5] = [16, 24, 32, 48, 256];

fn main() {
  println!("cargo:rerun-if-changed=src/app_icon.rs");
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=../build-support/version_info.rs");

  // For the About card, the log and the exe's version resource. `scripts/`: the installer scripts
  // are built into the exe.
  let version = version_info::version(&[".", "../../scripts"]);
  println!("cargo:rustc-env=GAMERSEQUALIZER_VERSION={version}");
  let version =
    VersionInfo { kind: FileKind::App, description: "GamersEqualizer", internal_name: "gamersequalizer" }.rc(&version);

  #[expect(clippy::expect_used, reason = "build script: a loud failure is the right outcome")]
  let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
  let ico_path = out_dir.join("app.ico");
  let rc_path = out_dir.join("app.rc");
  #[expect(clippy::expect_used, reason = "build script: a loud failure is the right outcome")]
  {
    std::fs::write(&ico_path, ico_bytes()).expect("write app.ico");
    let ico = ico_path.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(&rc_path, format!("{version}1 ICON \"{ico}\"\n")).expect("write app.rc");
  }
  embed_resource::compile(&rc_path, embed_resource::NONE).manifest_required().unwrap_or_else(|e| {
    // No resource compiler (no Windows SDK): build without the icon rather than fail.
    println!("cargo:warning=exe icon and version not embedded: {e}");
  });
}

/// ICO with one 32-bit BMP image per size (alpha in the pixels, empty AND mask).
fn ico_bytes() -> Vec<u8> {
  let images: Vec<Vec<u8>> = ICO_SIZES.iter().map(|&s| bmp_entry(s, &app_icon::rgba(s, false, false, 1.0))).collect();
  let mut out = Vec::new();
  out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
  out.extend_from_slice(&(ICO_SIZES.len() as u16).to_le_bytes());
  let mut offset = 6 + 16 * ICO_SIZES.len() as u32;
  for (&size, image) in ICO_SIZES.iter().zip(&images) {
    let dim = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
    out.extend_from_slice(&[dim, dim, 0, 0]);
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&(image.len() as u32).to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    offset += image.len() as u32;
  }
  for image in images {
    out.extend_from_slice(&image);
  }
  out
}

fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
  let mask_row = size.div_ceil(32) * 4; // 1 bit per pixel, rows padded to 4 bytes
  let mut out = Vec::new();
  out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
  out.extend_from_slice(&(size as i32).to_le_bytes());
  out.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // XOR image + AND mask
  out.extend_from_slice(&1u16.to_le_bytes());
  out.extend_from_slice(&32u16.to_le_bytes());
  out.extend_from_slice(&[0; 24]); // no compression, sizes/resolution/palette unused
  // Rows bottom-up, pixels as BGRA.
  for row in rgba.chunks_exact(size as usize * 4).rev() {
    for &[r, g, b, a] in row.as_chunks::<4>().0 {
      out.extend_from_slice(&[b, g, r, a]);
    }
  }
  out.resize(out.len() + (mask_row * size) as usize, 0);
  out
}
