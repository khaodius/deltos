//! How a downloaded package is put in place, which depends on how the app was installed:
//!
//! - **Windows, installed**: the setup program, run with `/VERYSILENT`. It closes the app,
//!   upgrades in place and, when asked with `/RESTART<APP>=1`, starts the app again.
//! - **Windows, portable** and **Linux, per-user** (`~/.local/bin`): the archive is unpacked
//!   and the new binary replaces the running one by rename, which works while it runs.
//! - **Linux, AppImage**: the new image is written next to the running one and renamed over
//!   it, keeping the path the person launches.
//! - **Linux, .deb or .rpm**: the package through apt, dnf, zypper or rpm, via `pkexec`, so the
//!   system asks for the password the way any package update does.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Downloaded, Error, Result};

/// How the running copy was installed, which decides which package to fetch and how to apply it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallKind {
    /// The Inno Setup installer, in `%LOCALAPPDATA%\Programs\<app>`.
    WindowsSetup,
    /// The portable zip, unpacked anywhere.
    WindowsPortable,
    /// The `.deb` package, owned by dpkg.
    Deb,
    /// The `.rpm` package, owned by rpm.
    Rpm,
    /// An AppImage; the `APPIMAGE` variable names the file.
    AppImage,
    /// The `.tar.gz` unpacked for one user, usually in `~/.local/bin`.
    LinuxUser,
}

/// What the app does after `install` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The new version is in place: start the program at this path and quit.
    Restart(PathBuf),
    /// The installer is running and starts the app itself once done: quit now.
    Quit,
}

/// Whether the app should come back after the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relaunch {
    /// The person asked to update now; the app restarts when the files are in place.
    Now,
    /// The app is quitting; put the files in place and leave it closed.
    OnExit,
}

/// The release file for `kind`, as the release workflow names it.
pub fn asset_name(kind: InstallKind, app_name: &str, version: &str) -> String {
    match kind {
        InstallKind::WindowsSetup => format!("{app_name}-{version}-x64-setup.exe"),
        InstallKind::WindowsPortable => format!("{app_name}-{version}-x64-windows.zip"),
        InstallKind::Deb => format!("{app_name}_{version}_amd64.deb"),
        InstallKind::Rpm => format!("{app_name}-{version}-1.x86_64.rpm"),
        InstallKind::AppImage => format!("{app_name}-{version}-x86_64.AppImage"),
        InstallKind::LinuxUser => format!("{app_name}-{version}-x86_64-linux.tar.gz"),
    }
}

/// Detects how the running copy was installed. On Linux this may ask dpkg or rpm, so call it
/// off the UI thread.
pub fn detect_install(app_name: &str) -> InstallKind {
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| exe.canonicalize().unwrap_or(exe));
    classify(
        exe.as_deref(),
        std::env::var_os("APPIMAGE").is_some(),
        cfg!(windows),
        || {
            exe.as_deref()
                .and_then(Path::parent)
                .is_some_and(|dir| dir.join("unins000.exe").exists())
        },
        || package_owns("dpkg-query", &["-W", app_name]),
        || package_owns("rpm", &["-q", app_name]),
    )
}

fn classify(
    exe: Option<&Path>,
    appimage: bool,
    windows: bool,
    has_uninstaller: impl FnOnce() -> bool,
    dpkg_owned: impl FnOnce() -> bool,
    rpm_owned: impl FnOnce() -> bool,
) -> InstallKind {
    if windows {
        // The setup program writes unins000.exe next to the app.
        return if has_uninstaller() {
            InstallKind::WindowsSetup
        } else {
            InstallKind::WindowsPortable
        };
    }
    if appimage {
        return InstallKind::AppImage;
    }
    if exe.is_some_and(|exe| exe.starts_with("/usr")) {
        if dpkg_owned() {
            return InstallKind::Deb;
        }
        if rpm_owned() {
            return InstallKind::Rpm;
        }
    }
    InstallKind::LinuxUser
}

fn package_owns(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn have(cmd: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .status()
        .map_err(|e| Error::Install(format!("could not run {program}: {e}")))?;
    if !status.success() {
        return Err(Error::Install(format!("{program} failed ({status})")));
    }
    Ok(())
}

fn binary_name(app_name: &str) -> String {
    if cfg!(windows) {
        format!("{app_name}.exe")
    } else {
        app_name.to_owned()
    }
}

/// Applies the downloaded package of `downloaded.install` to the running copy.
pub(crate) fn apply(
    downloaded: &Downloaded,
    app_name: &str,
    relaunch: Relaunch,
) -> Result<Applied> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Install(format!("could not find the running program: {e}")))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let bin_dir = exe
        .parent()
        .ok_or_else(|| Error::Install("the running program has no folder".into()))?;
    let file = &downloaded.path;
    let dir = &downloaded.dir;
    match downloaded.install {
        InstallKind::WindowsSetup => {
            let mut cmd = Command::new(file);
            cmd.args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
            ]);
            if relaunch == Relaunch::Now {
                // The installer script starts the app again when it sees this flag.
                cmd.arg(format!("/RESTART{}=1", app_name.to_ascii_uppercase()));
            }
            cmd.spawn()
                .map_err(|e| Error::Install(format!("could not start the setup program: {e}")))?;
            Ok(Applied::Quit)
        }
        InstallKind::WindowsPortable => {
            let out = dir.join("unpacked");
            run(Command::new("powershell").args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                    file.display(),
                    out.display()
                ),
            ]))?;
            let unpacked = first_dir(&out, app_name)?;
            let name = binary_name(app_name);
            replace(&unpacked.join(&name), &bin_dir.join(&name))?;
            Ok(Applied::Restart(exe))
        }
        InstallKind::LinuxUser => {
            run(Command::new("tar").arg("-xzf").arg(file).arg("-C").arg(dir))?;
            let unpacked = first_dir(dir, app_name)?;
            replace(&unpacked.join(app_name), &bin_dir.join(app_name))?;
            Ok(Applied::Restart(exe))
        }
        InstallKind::AppImage => {
            let target = std::env::var_os("APPIMAGE")
                .map(PathBuf::from)
                .ok_or_else(|| Error::Install("APPIMAGE is not set".into()))?;
            replace_appimage(file, &target)?;
            Ok(Applied::Restart(target))
        }
        InstallKind::Deb => {
            let mut cmd = Command::new("pkexec");
            if have("apt-get") {
                cmd.args([
                    "env",
                    "DEBIAN_FRONTEND=noninteractive",
                    "apt-get",
                    "install",
                    "-y",
                ]);
            } else {
                cmd.args(["dpkg", "-i"]);
            }
            cmd.arg(file);
            // apt reads local packages as its own user, so the file must be readable to it.
            make_readable(dir, file);
            run(&mut cmd)?;
            Ok(Applied::Restart(exe))
        }
        InstallKind::Rpm => {
            let mut cmd = Command::new("pkexec");
            if have("dnf") {
                cmd.args(["dnf", "install", "-y"]);
            } else if have("zypper") {
                cmd.args([
                    "zypper",
                    "--non-interactive",
                    "install",
                    "--allow-unsigned-rpm",
                ]);
            } else {
                cmd.args(["rpm", "-U"]);
            }
            cmd.arg(file);
            run(&mut cmd)?;
            Ok(Applied::Restart(exe))
        }
    }
}

/// Moves `new` over `target`, keeping the old file aside until the next start, so a binary
/// that is running can be replaced.
fn replace(new: &Path, target: &Path) -> Result<()> {
    if !new.is_file() {
        return Err(Error::Install(format!(
            "the package has no {}",
            new.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    let old = target.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if target.exists() {
        std::fs::rename(target, &old)?;
    }
    if let Err(e) = std::fs::copy(new, target) {
        let _ = std::fs::rename(&old, target);
        return Err(e.into());
    }
    set_executable(target)?;
    Ok(())
}

/// Writes the new image beside the running one and renames it over, so the path the person
/// starts stays the same and the running image, still mapped, is untouched.
fn replace_appimage(new: &Path, target: &Path) -> Result<()> {
    let partial = partial_path(target)
        .ok_or_else(|| Error::Install(format!("{} is not a file path", target.display())))?;
    let _ = std::fs::remove_file(&partial);
    std::fs::copy(new, &partial)?;
    set_executable(&partial)?;
    if let Err(e) = std::fs::rename(&partial, target) {
        let _ = std::fs::remove_file(&partial);
        return Err(e.into());
    }
    Ok(())
}

/// `dir/.name.partial` for `dir/name`.
fn partial_path(target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?.to_string_lossy();
    Some(target.with_file_name(format!(".{name}.partial")))
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn make_readable(dir: &Path, file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    let _ = (dir, file);
}

/// The `<app>-<version>-…` folder an archive unpacks into.
fn first_dir(dir: &Path, app_name: &str) -> Result<PathBuf> {
    let prefix = format!("{app_name}-");
    std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .ok_or_else(|| Error::Install(format!("the archive has no {prefix} folder")))
}

/// Removes what the previous update left behind: the replaced binary, a half-written AppImage
/// and the downloads.
pub(crate) fn clean_up(app_name: &str) {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        let _ = std::fs::remove_file(dir.join(format!("{app_name}.old")));
    }
    if let Some(partial) = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .and_then(|p| partial_path(&p))
    {
        let _ = std::fs::remove_file(partial);
    }
    let prefix = format!("{app_name}-update-");
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.filter_map(std::result::Result::ok) {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_assets_like_the_release_workflow() {
        assert_eq!(
            asset_name(InstallKind::WindowsSetup, "deltos", "0.1.2"),
            "deltos-0.1.2-x64-setup.exe"
        );
        assert_eq!(
            asset_name(InstallKind::WindowsPortable, "deltos", "0.1.2"),
            "deltos-0.1.2-x64-windows.zip"
        );
        assert_eq!(
            asset_name(InstallKind::Deb, "deltos", "0.1.2"),
            "deltos_0.1.2_amd64.deb"
        );
        assert_eq!(
            asset_name(InstallKind::Rpm, "deltos", "0.1.2"),
            "deltos-0.1.2-1.x86_64.rpm"
        );
        assert_eq!(
            asset_name(InstallKind::AppImage, "deltos", "0.1.2"),
            "deltos-0.1.2-x86_64.AppImage"
        );
        assert_eq!(
            asset_name(InstallKind::LinuxUser, "deltos", "0.1.2"),
            "deltos-0.1.2-x86_64-linux.tar.gz"
        );
    }

    fn kind(
        exe: &str,
        appimage: bool,
        windows: bool,
        uninstaller: bool,
        dpkg: bool,
        rpm: bool,
    ) -> InstallKind {
        classify(
            Some(Path::new(exe)),
            appimage,
            windows,
            || uninstaller,
            || dpkg,
            || rpm,
        )
    }

    #[test]
    fn classifies_installs() {
        assert_eq!(
            kind(
                "C:\\Users\\a\\AppData\\Local\\Programs\\deltos\\deltos.exe",
                false,
                true,
                true,
                false,
                false
            ),
            InstallKind::WindowsSetup
        );
        assert_eq!(
            kind(
                "D:\\tools\\deltos\\deltos.exe",
                false,
                true,
                false,
                false,
                false
            ),
            InstallKind::WindowsPortable
        );
        assert_eq!(
            kind(
                "/tmp/.mount_deltosAbc/usr/bin/deltos",
                true,
                false,
                false,
                false,
                false
            ),
            InstallKind::AppImage
        );
        assert_eq!(
            kind("/usr/bin/deltos", false, false, false, true, false),
            InstallKind::Deb
        );
        assert_eq!(
            kind("/usr/bin/deltos", false, false, false, false, true),
            InstallKind::Rpm
        );
        // Under /usr but owned by no package manager: a manual copy, treated as a user install.
        assert_eq!(
            kind("/usr/local/bin/deltos", false, false, false, false, false),
            InstallKind::LinuxUser
        );
        assert_eq!(
            kind("/home/a/.local/bin/deltos", false, false, false, true, true),
            InstallKind::LinuxUser
        );
        assert_eq!(
            classify(None, false, false, || false, || false, || false),
            InstallKind::LinuxUser
        );
    }

    #[test]
    fn package_queries_run_only_when_needed() {
        let asked = std::cell::Cell::new(false);
        let kind = classify(
            Some(Path::new("/home/a/.local/bin/deltos")),
            false,
            false,
            || false,
            || {
                asked.set(true);
                true
            },
            || {
                asked.set(true);
                true
            },
        );
        assert_eq!(kind, InstallKind::LinuxUser);
        assert!(!asked.get());
    }

    #[test]
    fn partial_sits_beside_the_appimage() {
        assert_eq!(
            partial_path(Path::new("/home/a/Apps/deltos-0.1.0-x86_64.AppImage")),
            Some(PathBuf::from(
                "/home/a/Apps/.deltos-0.1.0-x86_64.AppImage.partial"
            ))
        );
    }

    #[test]
    fn replaces_a_file_and_keeps_the_old_one() {
        let dir = std::env::temp_dir().join(format!("deltos-updater-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("deltos");
        let new = dir.join("new");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace(&new, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("deltos.old")).unwrap(), b"old");
        assert!(replace(&dir.join("missing"), &target).is_err());

        let image = dir.join("deltos-0.1.0-x86_64.AppImage");
        std::fs::write(&image, b"v1").unwrap();
        replace_appimage(&new, &image).unwrap();
        assert_eq!(std::fs::read(&image).unwrap(), b"new");
        assert!(!partial_path(&image).unwrap().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
