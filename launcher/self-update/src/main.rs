#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

const APPLICATION_MANIFEST_FILE: &str = ".application-manifest.json";
const APPLICATION_VERSION_FILE: &str = ".application-version";
const LAUNCHER_FILE_NAME: &str = "rebellion2-launcher.exe";
const PENDING_APPLICATION_MANIFEST_FILE: &str = ".application-manifest.pending.json";
const PENDING_APPLICATION_VERSION_FILE: &str = ".application-version.pending";
const STAGED_LAUNCHER_FILE_NAME: &str = ".rebellion2-launcher.next.exe";

#[cfg(target_os = "windows")]
const UNINSTALL_REGISTRY_KEY: &str = concat!(
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\",
    r"{7C3F1E92-5A4B-4D8E-9F21-3B6C8A2D4E10}_is1"
);

fn main() {
    let relaunch = std::env::args().any(|argument| argument == "--relaunch");
    if let Err(error) = complete_update(relaunch) {
        report_error(&error.to_string());
        std::process::exit(1);
    }
}

/// Waits for the installed launcher to exit, promotes all staged update files, and
/// optionally reopens the launcher.
fn complete_update(relaunch: bool) -> Result<(), Box<dyn std::error::Error>> {
    let install_dir = find_install_dir()?;
    validate_install_dir(&install_dir)?;
    let launcher_path = install_dir.join(LAUNCHER_FILE_NAME);
    let staged_launcher_path = install_dir.join(STAGED_LAUNCHER_FILE_NAME);

    wait_for_launcher_exit(&launcher_path);
    if staged_launcher_path.is_file() {
        replace_file(&staged_launcher_path, &launcher_path)?;
    }
    promote_pending_file(
        &install_dir.join(PENDING_APPLICATION_MANIFEST_FILE),
        &install_dir.join(APPLICATION_MANIFEST_FILE),
    )?;
    promote_pending_file(
        &install_dir.join(PENDING_APPLICATION_VERSION_FILE),
        &install_dir.join(APPLICATION_VERSION_FILE),
    )?;
    update_installed_version(&install_dir)?;

    if relaunch {
        Command::new(&launcher_path)
            .current_dir(&install_dir)
            .spawn()?;
    }
    Ok(())
}

/// Replaces a managed file when a pending version exists.
fn promote_pending_file(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_file() {
        replace_file(source, destination)?;
    }
    Ok(())
}

/// Waits until Windows releases its write lock on the running launcher executable.
fn wait_for_launcher_exit(launcher_path: &Path) {
    while fs::OpenOptions::new()
        .write(true)
        .open(launcher_path)
        .is_err()
    {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Verifies that the registry target still looks like a Rebellion 2 installation.
fn validate_install_dir(install_dir: &Path) -> io::Result<()> {
    if !install_dir.join(LAUNCHER_FILE_NAME).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "installed launcher was not found",
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
    fs::rename(source, destination)
}

/// Returns the installation directory containing the running update helper.
fn find_install_dir() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    install_dir_from_executable(&executable)
}

/// Returns the parent directory of an update-helper executable.
fn install_dir_from_executable(executable: &Path) -> io::Result<PathBuf> {
    executable.parent().map(Path::to_path_buf).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "update helper has no parent directory",
        )
    })
}

#[cfg(target_os = "windows")]
/// Updates Add or Remove Programs after the pending application version is promoted.
fn update_installed_version(install_dir: &Path) -> io::Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(uninstall_key) = current_user.open_subkey_with_flags(
        UNINSTALL_REGISTRY_KEY,
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    ) else {
        return Ok(());
    };
    let Ok(registered_install_dir) = uninstall_key.get_value::<String, _>("InstallLocation") else {
        return Ok(());
    };
    if !paths_refer_to_same_directory(install_dir, Path::new(&registered_install_dir)) {
        return Ok(());
    }

    let version = fs::read_to_string(install_dir.join(APPLICATION_VERSION_FILE))?;
    let version = version.trim();
    uninstall_key.set_value("DisplayVersion", &version)?;
    uninstall_key.set_value("DisplayName", &format!("Rebellion 2 version {version}"))?;
    Ok(())
}

#[cfg(target_os = "windows")]
/// Returns whether two paths resolve to the same directory.
fn paths_refer_to_same_directory(first: &Path, second: &Path) -> bool {
    match (fs::canonicalize(first), fs::canonicalize(second)) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

#[cfg(not(target_os = "windows"))]
fn update_installed_version(_install_dir: &Path) -> io::Result<()> {
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
    fn validate_install_dir_without_launcher_returns_error() {
        let directory = tempfile::tempdir().unwrap();

        assert!(validate_install_dir(directory.path()).is_err());
    }

    #[test]
    fn install_dir_from_executable_returns_executable_parent() {
        let executable = Path::new("installation").join("rebellion2-update-helper.exe");

        assert_eq!(
            install_dir_from_executable(&executable).unwrap(),
            Path::new("installation")
        );
    }
}
