#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

const LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.json";
const LAUNCHER_VERSION_FILE: &str = ".launcher-version";
const PENDING_LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.pending.json";
const PENDING_LAUNCHER_VERSION_FILE: &str = ".launcher-version.pending";
const APPLICATION_MANIFEST_FILE: &str = ".application-manifest.json";
const APPLICATION_VERSION_FILE: &str = ".application-version";
const PENDING_APPLICATION_MANIFEST_FILE: &str = ".application-manifest.pending.json";
const PENDING_APPLICATION_VERSION_FILE: &str = ".application-version.pending";
const LAUNCHER_BACKUP_FILE: &str = ".rebellion2-launcher.backup";

struct Arguments {
    launcher: PathBuf,
    staged: PathBuf,
    metadata_dir: PathBuf,
    wait_pid: u32,
    relaunch: bool,
}

fn main() {
    let result = parse_arguments(env::args_os().skip(1)).and_then(complete_update);
    if let Err(error) = result {
        report_error(&error.to_string());
        std::process::exit(1);
    }
}

/// Parses the explicit paths supplied by the launcher being replaced.
fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> io::Result<Arguments> {
    let mut launcher = None;
    let mut staged = None;
    let mut metadata_dir = None;
    let mut wait_pid = None;
    let mut relaunch = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--launcher") => launcher = arguments.next().map(PathBuf::from),
            Some("--staged") => staged = arguments.next().map(PathBuf::from),
            Some("--metadata-dir") => metadata_dir = arguments.next().map(PathBuf::from),
            Some("--wait-pid") => {
                wait_pid = arguments
                    .next()
                    .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
            }
            Some("--relaunch") => relaunch = true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unknown launcher-update argument",
                ))
            }
        }
    }
    Ok(Arguments {
        launcher: launcher.ok_or_else(|| missing_argument("--launcher"))?,
        staged: staged.ok_or_else(|| missing_argument("--staged"))?,
        metadata_dir: metadata_dir.ok_or_else(|| missing_argument("--metadata-dir"))?,
        wait_pid: wait_pid.ok_or_else(|| missing_argument("--wait-pid"))?,
        relaunch,
    })
}

/// Creates a consistent missing-argument error.
fn missing_argument(argument: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("missing {argument} launcher-update argument"),
    )
}

/// Waits for the launcher to exit, promotes the staged launcher metadata, and
/// optionally reopens the launcher.
fn complete_update(arguments: Arguments) -> io::Result<()> {
    validate_paths(&arguments)?;
    wait_for_launcher_exit(arguments.wait_pid, &arguments.launcher);
    replace_and_resign_launcher(&arguments, resign_macos_bundle)?;
    promote_pending_file(
        &arguments.metadata_dir.join(PENDING_LAUNCHER_MANIFEST_FILE),
        &arguments.metadata_dir.join(LAUNCHER_MANIFEST_FILE),
    )?;
    promote_pending_file(
        &arguments.metadata_dir.join(PENDING_LAUNCHER_VERSION_FILE),
        &arguments.metadata_dir.join(LAUNCHER_VERSION_FILE),
    )?;
    promote_pending_file(
        &arguments
            .metadata_dir
            .join(PENDING_APPLICATION_MANIFEST_FILE),
        &arguments.metadata_dir.join(APPLICATION_MANIFEST_FILE),
    )?;
    promote_pending_file(
        &arguments
            .metadata_dir
            .join(PENDING_APPLICATION_VERSION_FILE),
        &arguments.metadata_dir.join(APPLICATION_VERSION_FILE),
    )?;
    if arguments.relaunch {
        Command::new(&arguments.launcher)
            .current_dir(
                arguments
                    .launcher
                    .parent()
                    .unwrap_or(&arguments.metadata_dir),
            )
            .spawn()?;
    }
    Ok(())
}

/// Replaces the launcher and restores the previous executable if bundle signing fails.
fn replace_and_resign_launcher(
    arguments: &Arguments,
    resign: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let backup = arguments.metadata_dir.join(LAUNCHER_BACKUP_FILE);
    if let Err(error) = fs::remove_file(&backup) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    fs::copy(&arguments.launcher, &backup)?;
    if let Err(error) = replace_file(&arguments.staged, &arguments.launcher) {
        let _ = fs::remove_file(&backup);
        return Err(error);
    }
    if let Err(signing_error) = resign(&arguments.launcher) {
        if let Err(rollback_error) = replace_file(&backup, &arguments.launcher) {
            return Err(io::Error::other(format!(
                "{signing_error}; restoring the previous launcher also failed: {rollback_error}"
            )));
        }
        return Err(signing_error);
    }
    fs::remove_file(backup)
}

/// Replaces a managed file when a pending version exists.
fn promote_pending_file(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_file() {
        replace_file(source, destination)?;
    }
    Ok(())
}

/// Waits until the process that started this helper has exited.
fn wait_for_launcher_exit(process_id: u32, launcher_path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = process_id;
    #[cfg(target_os = "windows")]
    while fs::OpenOptions::new()
        .write(true)
        .open(launcher_path)
        .is_err()
    {
        thread::sleep(Duration::from_millis(50));
    }
    #[cfg(not(target_os = "windows"))]
    while Command::new("/bin/kill")
        .arg("-0")
        .arg(process_id.to_string())
        .status()
        .is_ok_and(|status| status.success())
    {
        thread::sleep(Duration::from_millis(50));
    }
    #[cfg(not(target_os = "windows"))]
    let _ = launcher_path;
}

/// Verifies that every caller-provided path is a concrete expected file location.
fn validate_paths(arguments: &Arguments) -> io::Result<()> {
    if !arguments.launcher.is_file() || !arguments.staged.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "installed or staged launcher was not found",
        ));
    }
    if !arguments.metadata_dir.is_dir()
        || arguments.staged.parent() != Some(arguments.metadata_dir.as_path())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launcher update paths do not share the metadata directory",
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
/// Atomically replaces a staged file while allowing an existing destination.
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = OsStr::new(source).encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = OsStr::new(destination)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            let replacement = destination.with_extension("launcher-update-replacement");
            if let Err(error) = fs::remove_file(&replacement) {
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            fs::copy(source, &replacement)?;
            fs::rename(&replacement, destination)?;
            fs::remove_file(source)
        }
    }
}

#[cfg(target_os = "macos")]
/// Restores an ad-hoc signature after replacing the launcher inside its app bundle.
fn resign_macos_bundle(launcher: &Path) -> io::Result<()> {
    let bundle = launcher
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid app bundle path"))?;
    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(bundle)
        .status()?;
    if !status.success() {
        return Err(io::Error::other("could not sign the updated app bundle"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn resign_macos_bundle(_launcher: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
/// Displays a native error because the helper has no persistent user interface.
fn report_error(message: &str) {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let title: Vec<u16> = OsStr::new("Rebellion 2 update failed")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let body: Vec<u16> = OsStr::new(message).encode_wide().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn report_error(message: &str) {
    eprintln!("Rebellion 2 update failed: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_pending_file_with_pending_file_replaces_destination() {
        let directory = tempfile::tempdir().unwrap();
        let pending = directory.path().join("pending");
        let destination = directory.path().join("destination");
        fs::write(&pending, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();

        promote_pending_file(&pending, &destination).unwrap();

        assert_eq!(fs::read(destination).unwrap(), b"new");
        assert!(!pending.exists());
    }

    #[test]
    fn promote_pending_file_without_pending_file_keeps_destination() {
        let directory = tempfile::tempdir().unwrap();
        let pending = directory.path().join("pending");
        let destination = directory.path().join("destination");
        fs::write(&destination, b"current").unwrap();

        promote_pending_file(&pending, &destination).unwrap();

        assert_eq!(fs::read(destination).unwrap(), b"current");
    }

    #[test]
    fn replace_and_resign_launcher_with_success_keeps_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("launcher");
        let staged = directory.path().join("staged");
        fs::write(&launcher, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let arguments = Arguments {
            launcher: launcher.clone(),
            staged,
            metadata_dir: directory.path().to_path_buf(),
            wait_pid: 1,
            relaunch: false,
        };

        replace_and_resign_launcher(&arguments, |_| Ok(())).unwrap();

        assert_eq!(fs::read(launcher).unwrap(), b"new");
        assert!(!directory.path().join(LAUNCHER_BACKUP_FILE).exists());
    }

    #[test]
    fn replace_and_resign_launcher_with_signing_failure_restores_previous_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("launcher");
        let staged = directory.path().join("staged");
        fs::write(&launcher, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let arguments = Arguments {
            launcher: launcher.clone(),
            staged,
            metadata_dir: directory.path().to_path_buf(),
            wait_pid: 1,
            relaunch: false,
        };

        let result =
            replace_and_resign_launcher(&arguments, |_| Err(io::Error::other("signing failed")));

        assert!(result.is_err());
        assert_eq!(fs::read(launcher).unwrap(), b"old");
        assert!(!directory.path().join(LAUNCHER_BACKUP_FILE).exists());
    }

    #[test]
    fn validate_paths_without_launcher_returns_error() {
        let directory = tempfile::tempdir().unwrap();
        let arguments = Arguments {
            launcher: directory.path().join("launcher"),
            staged: directory.path().join("staged"),
            metadata_dir: directory.path().to_path_buf(),
            wait_pid: 1,
            relaunch: false,
        };

        assert!(validate_paths(&arguments).is_err());
    }

    #[test]
    fn parse_arguments_with_required_paths_returns_arguments() {
        let arguments = parse_arguments(
            [
                "--launcher",
                "installation/launcher",
                "--staged",
                "metadata/staged",
                "--metadata-dir",
                "metadata",
                "--wait-pid",
                "42",
                "--relaunch",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .unwrap();

        assert_eq!(arguments.launcher, Path::new("installation/launcher"));
        assert_eq!(arguments.staged, Path::new("metadata/staged"));
        assert_eq!(arguments.metadata_dir, Path::new("metadata"));
        assert_eq!(arguments.wait_pid, 42);
        assert!(arguments.relaunch);
    }

    #[test]
    fn parse_arguments_without_wait_pid_returns_error() {
        let result = parse_arguments(
            [
                "--launcher",
                "installation/launcher",
                "--staged",
                "metadata/staged",
                "--metadata-dir",
                "metadata",
            ]
            .into_iter()
            .map(OsString::from),
        );

        assert!(result.is_err());
    }
}
