//! Where the app and its DLL live: installed, bundled next to the exe, or a dev build.

use super::*;

const DLL_NAME: &str = "gamersequalizer_apo.dll";
/// The app itself, installed next to its effects: `%ProgramFiles%\GamersEqualizer\<this>`.
const EXE_NAME: &str = "gamersequalizer.exe";

/// The installed app to start instead of this process: this exe runs from elsewhere (the
/// unzipped folder, an old autostart entry) and the installed copy is the same build or
/// newer. None in the dev tree, for the installed copy itself, and while the installed DLL is
/// missing (removed by an antivirus, a half-done uninstall): the installed copy could not put
/// it back, a copy from the ZIP can.
pub fn handover_target() -> Option<PathBuf> {
  let this = std::env::current_exe().ok()?;
  let installed = installed_exe().filter(|_| installed_dll().is_some())?;
  if same_path(&this, &installed) || in_dev_tree(this.parent()?) {
    return None;
  }
  let installed_at = file_stamp(&installed)?;
  let this_at = file_stamp(&this)?;
  (installed_at >= this_at).then_some(installed)
}

/// The exe "start with Windows" should run: the installed copy once there is one.
pub fn autostart_exe() -> Option<PathBuf> {
  let this = std::env::current_exe().ok()?;
  let installed = installed_exe().filter(|_| !this.parent().is_some_and(in_dev_tree));
  Some(installed.unwrap_or(this))
}

/// This exe still has to be installed: (installed copy's build time if any, this exe's).
/// None in the dev tree, for the installed copy itself and for an older or equal build.
pub(super) fn app_to_install() -> Option<(Option<SystemTime>, SystemTime)> {
  let this = std::env::current_exe().ok()?;
  let target = installed_exe_path()?;
  if same_path(&this, &target) || in_dev_tree(this.parent()?) {
    return None;
  }
  let this_at = file_stamp(&this)?;
  let installed_at = file_stamp(&target);
  installed_at.is_none_or(|t| this_at > t).then_some((installed_at, this_at))
}

pub(super) fn known_folder(id: &GUID) -> Option<PathBuf> {
  // SAFETY: on success the shell allocates the string; it is copied, then freed once.
  unsafe {
    let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
    let path = p.to_string();
    CoTaskMemFree(Some(p.0 as *const _));
    path.ok().map(PathBuf::from)
  }
}

/// `%ProgramFiles%\GamersEqualizer` by known folder, not the variable (the user can override it).
fn program_dir() -> Option<PathBuf> {
  Some(known_folder(&FOLDERID_ProgramFiles)?.join("GamersEqualizer"))
}

pub(super) fn installed_dll() -> Option<PathBuf> {
  let path = program_dir()?.join("apo").join(DLL_NAME);
  path.is_file().then_some(path)
}

/// Where the installer puts the app (whether or not it is there yet).
fn installed_exe_path() -> Option<PathBuf> {
  Some(program_dir()?.join(EXE_NAME))
}

pub(super) fn installed_exe() -> Option<PathBuf> {
  installed_exe_path().filter(|path| path.is_file())
}

/// `dir` is `target/<profile>/` of the source tree (`Cargo.toml` next to `target`): a
/// developer's build, which runs in place and is never installed.
pub(super) fn in_dev_tree(dir: &Path) -> bool {
  dir.parent().is_some_and(|target| {
    target.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target"))
      && target.parent().is_some_and(|root| root.join("Cargo.toml").is_file())
  })
}

/// Windows paths compare without case.
fn same_path(a: &Path, b: &Path) -> bool {
  a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// In the dev tree the `target/apo` build first: `target/release` may hold a stale DLL from a
/// plain `--release` build, which is not the unwinding `apo` profile. The installed app
/// reinstalls its installed DLL (a new device, another slot). Otherwise only next to the exe
/// (the unzipped folder): an exe in `Downloads\x\` must not pick up whatever sits in
/// `Downloads\apo\`.
pub(super) fn bundled_dll() -> Option<PathBuf> {
  let dir = exe_dir()?;
  if installed_exe_path().is_some_and(|installed| installed.parent().is_some_and(|p| same_path(p, &dir))) {
    return installed_dll();
  }
  let dev = in_dev_tree(&dir).then(|| dir.join("..").join("apo").join(DLL_NAME));
  dev.into_iter().chain([dir.join(DLL_NAME)]).find(|p| p.is_file())
}

fn exe_dir() -> Option<PathBuf> {
  std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

pub(super) fn file_stamp(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).ok()?.modified().ok()
}
