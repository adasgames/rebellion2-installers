#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::fs;
use std::{
    env,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
};
#[cfg(not(target_os = "windows"))]
use std::{thread, time::Duration};

use rebellion2_update_core::{recover_interrupted_apply, InstallationLock};

const LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.json";
const LAUNCHER_VERSION_FILE: &str = ".launcher-version";
const PENDING_LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.pending.json";
const PENDING_LAUNCHER_VERSION_FILE: &str = ".launcher-version.pending";
const APPLICATION_MANIFEST_FILE: &str = ".application-manifest.json";
const APPLICATION_VERSION_FILE: &str = ".application-version";
const PENDING_APPLICATION_MANIFEST_FILE: &str = ".application-manifest.pending.json";
const PENDING_APPLICATION_VERSION_FILE: &str = ".application-version.pending";
#[cfg(any(target_os = "macos", test))]
const LAUNCHER_BACKUP_FILE: &str = ".rebellion2-launcher.backup";
const LEGACY_WINDOWS_LAUNCHER_FILE: &str = "rebellion2-launcher.exe";
const LEGACY_WINDOWS_STAGED_LAUNCHER_FILE: &str = ".rebellion2-launcher.next.exe";

struct Arguments {
    launcher: PathBuf,
    staged: PathBuf,
    metadata_dir: PathBuf,
    wait_pid: u32,
    relaunch: bool,
}

fn main() {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--recover-installation")
    {
        let result = if arguments.len() == 2 {
            prepare_installation(Path::new(&arguments[1]))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected --recover-installation <directory>",
            ))
        };
        if let Err(error) = result {
            eprintln!("Installation recovery failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let result = parse_arguments(arguments).and_then(complete_update);
    if let Err(error) = result {
        report_error(&error.to_string());
        std::process::exit(1);
    }
}

/// Restores interrupted changes before setup replaces its application baseline.
/// Setup acquires the same lock after this helper exits and before changing files.
fn prepare_installation(directory: &Path) -> io::Result<()> {
    let _installation = InstallationLock::try_acquire(directory)?;
    #[cfg(target_os = "windows")]
    let _application_files = lock_application_files(directory)?;
    recover_interrupted_apply(directory)?;
    for name in [
        PENDING_LAUNCHER_MANIFEST_FILE,
        PENDING_LAUNCHER_VERSION_FILE,
        PENDING_APPLICATION_MANIFEST_FILE,
        PENDING_APPLICATION_VERSION_FILE,
        LEGACY_WINDOWS_STAGED_LAUNCHER_FILE,
    ] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Prevents recovery while an older launcher, helper, or game still uses this installation.
#[cfg(target_os = "windows")]
fn lock_application_files(directory: &Path) -> io::Result<Vec<fs::File>> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut files = Vec::new();
    for name in [
        LEGACY_WINDOWS_LAUNCHER_FILE,
        "Rebellion2.exe",
        "rebellion2-update-helper.exe",
    ] {
        match fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(directory.join(name))
        {
            Ok(file) => files.push(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(files)
}

/// Parses the explicit paths supplied by the launcher being replaced.
fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> io::Result<Arguments> {
    #[cfg(target_os = "windows")]
    let legacy_executable = Some(env::current_exe()?);
    #[cfg(not(target_os = "windows"))]
    let legacy_executable: Option<PathBuf> = None;
    parse_arguments_with_legacy_executable(arguments, legacy_executable.as_deref())
}

/// Parses explicit arguments or the fixed-path protocol used by legacy Windows launchers.
fn parse_arguments_with_legacy_executable(
    arguments: impl IntoIterator<Item = OsString>,
    legacy_executable: Option<&Path>,
) -> io::Result<Arguments> {
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
    if launcher.is_none() && staged.is_none() && metadata_dir.is_none() && wait_pid.is_none() {
        if let Some(executable) = legacy_executable {
            return legacy_windows_arguments(executable, relaunch);
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

/// Derives the fixed installation paths understood by the previous Windows launcher.
fn legacy_windows_arguments(executable: &Path, relaunch: bool) -> io::Result<Arguments> {
    let metadata_dir = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "legacy update helper has no parent directory",
        )
    })?;
    Ok(Arguments {
        launcher: metadata_dir.join(LEGACY_WINDOWS_LAUNCHER_FILE),
        staged: metadata_dir.join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE),
        metadata_dir: metadata_dir.to_path_buf(),
        wait_pid: 0,
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
    wait_for_launcher_exit(arguments.wait_pid)?;
    let installation = InstallationLock::acquire(&arguments.metadata_dir)?;
    validate_paths(&arguments)?;
    replace_launcher(&arguments)?;
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
    // Release ownership before the replacement launcher acquires it at startup.
    drop(installation);
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

#[cfg(not(target_os = "macos"))]
/// Replaces the launcher without creating a signing rollback backup.
fn replace_launcher(arguments: &Arguments) -> io::Result<()> {
    replace_file(&arguments.staged, &arguments.launcher)
}

#[cfg(target_os = "macos")]
/// Replaces and signs the launcher while retaining the previous executable for rollback.
fn replace_launcher(arguments: &Arguments) -> io::Result<()> {
    replace_and_resign_launcher(arguments, resign_macos_bundle)
}

#[cfg(any(target_os = "macos", test))]
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

/// Waits for the actual caller process instead of mistaking a file-access failure for a running launcher.
#[cfg(target_os = "windows")]
fn wait_for_launcher_exit(process_id: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
    };
    let process_id = if process_id == 0 {
        legacy_parent_process_id()?
    } else {
        process_id
    };
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let result = match unsafe { WaitForSingleObject(process, INFINITE) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        status => Err(io::Error::other(format!(
            "unexpected launcher wait result: {status}"
        ))),
    };
    unsafe {
        CloseHandle(process);
    }
    result
}

/// Resolves the caller for the legacy handoff protocol, which supplies no process identifier.
#[cfg(target_os = "windows")]
fn legacy_parent_process_id() -> io::Result<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut present = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    let mut parent = None;
    while present {
        if entry.th32ProcessID == std::process::id() {
            parent = Some(entry.th32ParentProcessID);
            break;
        }
        present = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    parent.filter(|parent| *parent != 0).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "launcher caller process was not found",
        )
    })
}

/// Waits until the process that started this helper has exited.
#[cfg(not(target_os = "windows"))]
fn wait_for_launcher_exit(process_id: u32) -> io::Result<()> {
    while Command::new("/bin/kill")
        .arg("-0")
        .arg(process_id.to_string())
        .status()
        .is_ok_and(|status| status.success())
    {
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
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
    rebellion2_update_core::replace_file_atomically(source, destination)
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
    fn prepare_installation_discards_pending_handoff_and_preserves_installed_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            PENDING_LAUNCHER_MANIFEST_FILE,
            PENDING_LAUNCHER_VERSION_FILE,
            PENDING_APPLICATION_MANIFEST_FILE,
            PENDING_APPLICATION_VERSION_FILE,
            LEGACY_WINDOWS_STAGED_LAUNCHER_FILE,
            LAUNCHER_MANIFEST_FILE,
            LAUNCHER_VERSION_FILE,
        ] {
            fs::write(directory.path().join(name), b"existing").unwrap();
        }
        prepare_installation(directory.path()).unwrap();
        assert!(!directory
            .path()
            .join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE)
            .exists());
        assert!(!directory
            .path()
            .join(PENDING_APPLICATION_VERSION_FILE)
            .exists());
        assert!(!directory
            .path()
            .join(PENDING_LAUNCHER_MANIFEST_FILE)
            .exists());
        assert_eq!(
            fs::read(directory.path().join(LAUNCHER_VERSION_FILE)).unwrap(),
            b"existing"
        );
        assert_eq!(
            fs::read(directory.path().join(LAUNCHER_MANIFEST_FILE)).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn prepare_installation_with_invalid_journal_preserves_pending_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join(".rebellion2-update-transaction");
        fs::create_dir(&journal).unwrap();
        fs::write(journal.join("transaction.json"), b"invalid").unwrap();
        fs::write(
            directory.path().join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE),
            b"staged",
        )
        .unwrap();
        assert!(prepare_installation(directory.path()).is_err());
        assert_eq!(
            fs::read(directory.path().join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE)).unwrap(),
            b"staged"
        );
    }

    #[test]
    fn prepare_installation_while_launcher_owns_installation_fails_before_changes() {
        let directory = tempfile::tempdir().unwrap();
        let _owner = InstallationLock::try_acquire(directory.path()).unwrap();
        fs::write(
            directory.path().join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE),
            b"staged",
        )
        .unwrap();
        assert_eq!(
            prepare_installation(directory.path()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(directory
            .path()
            .join(LEGACY_WINDOWS_STAGED_LAUNCHER_FILE)
            .exists());
    }

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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn replace_launcher_without_macos_signing_replaces_without_backup() {
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

        replace_launcher(&arguments).unwrap();

        assert_eq!(fs::read(launcher).unwrap(), b"new");
        assert!(!directory.path().join(LAUNCHER_BACKUP_FILE).exists());
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

    #[test]
    fn parse_arguments_with_legacy_windows_protocol_derives_fixed_installation_paths() {
        let arguments = parse_arguments_with_legacy_executable(
            ["--relaunch"].into_iter().map(OsString::from),
            Some(Path::new("installation/rebellion2-update-helper.exe")),
        )
        .unwrap();

        assert_eq!(
            arguments.launcher,
            Path::new("installation/rebellion2-launcher.exe")
        );
        assert_eq!(
            arguments.staged,
            Path::new("installation/.rebellion2-launcher.next.exe")
        );
        assert_eq!(arguments.metadata_dir, Path::new("installation"));
        assert_eq!(arguments.wait_pid, 0);
        assert!(arguments.relaunch);
    }
}
