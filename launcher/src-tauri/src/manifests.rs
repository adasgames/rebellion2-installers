#[cfg(target_os = "macos")]
use crate::MACOS_GAME_ARCHIVE_FILE_NAME;
use crate::{LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME};
use rebellion2_update_core::{validate_manifest, FileEntry, Manifest};
use std::io;
use std::path::{Component, Path};

/// Validates that a launcher manifest contains exactly the launcher and helper.
pub(crate) fn validate_launcher_manifest<'a>(
    manifest: &'a Manifest,
    version: &str,
) -> io::Result<(&'a FileEntry, &'a FileEntry)> {
    validate_manifest(manifest)?;
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher manifest version does not match its channel pointer",
        ));
    }
    if manifest.files.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher manifest must contain exactly two files",
        ));
    }
    let launcher = manifest
        .files
        .iter()
        .find(|entry| entry.path == LAUNCHER_FILE_NAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "launcher is missing"))?;
    let helper = manifest
        .files
        .iter()
        .find(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "launcher helper is missing"))?;
    Ok((launcher, helper))
}

/// Validates the single signed archive used for macOS game replacement.
#[cfg(target_os = "macos")]
pub(crate) fn validate_macos_game_archive_manifest(manifest: &Manifest) -> io::Result<&FileEntry> {
    if manifest.files.len() != 1 || manifest.files[0].path != MACOS_GAME_ARCHIVE_FILE_NAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS game manifest must contain exactly one game archive",
        ));
    }
    Ok(&manifest.files[0])
}

/// Validates that a game manifest cannot overwrite launcher or content files.
pub(crate) fn validate_game_manifest(manifest: &Manifest, version: &str) -> io::Result<()> {
    validate_manifest(manifest)?;
    if manifest.version != version || manifest.files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "game manifest version or file list is invalid",
        ));
    }
    for entry in &manifest.files {
        let path = Path::new(&entry.path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || entry.path.eq_ignore_ascii_case(LAUNCHER_FILE_NAME)
            || entry.path.eq_ignore_ascii_case(UPDATE_HELPER_FILE_NAME)
            || is_update_state_path(path)
            || is_unmanaged_application_path(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "game manifest contains a launcher, content, or unsafe path",
            ));
        }
    }
    Ok(())
}

/// Identifies local update state that downloaded file inventories must not own.
fn is_update_state_path(path: &Path) -> bool {
    let Some(Component::Normal(component)) = path.components().next() else {
        return false;
    };
    matches!(
        component.to_string_lossy().to_ascii_lowercase().as_str(),
        ".launcher-version"
            | ".game-version"
            | ".game-manifest.json"
            | ".application-version"
            | ".application-manifest.json"
            | ".application-version.pending"
            | ".application-manifest.pending.json"
            | ".approved-content-update"
            | ".content-packs"
            | "content.zip"
            | "content.zip.part"
            | "content.zip.part.etag"
            | "content.zip.part.size"
            | ".launcher-manifest.json"
            | ".launcher-version.pending"
            | ".launcher-resume.json"
            | ".launcher-manifest.pending.json"
            | ".rebellion2-launcher.next.exe"
            | ".rebellion2-launcher.next"
            | ".rebellion2-launcher.next.appimage"
            | ".rebellion2-update-transaction"
            | ".rebellion2-install.lock"
            | ".content-install-staging"
            | ".download-cache"
            | ".session"
            | "launcher.log"
    )
}

/// Ensures an application manifest cannot modify content, mods, or paths outside
/// the installation directory.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn validate_application_manifest(manifest: &Manifest, version: &str) -> io::Result<()> {
    validate_manifest(manifest)?;
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest version does not match its channel pointer",
        ));
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == LAUNCHER_FILE_NAME)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest does not contain the launcher",
        ));
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest does not contain the update helper",
        ));
    }
    for entry in &manifest.files {
        let path = Path::new(&entry.path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || is_unmanaged_application_path(path)
            || is_update_state_path(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "application manifest contains an unmanaged path",
            ));
        }
    }
    Ok(())
}

/// Returns whether a path belongs to player-managed content rather than the application.
fn is_unmanaged_application_path(path: &Path) -> bool {
    let Some(Component::Normal(component)) = path.components().next() else {
        return false;
    };
    let component = component.to_string_lossy();
    component.eq_ignore_ascii_case("Content") || component.eq_ignore_ascii_case("Mods")
}

pub(crate) fn validate_base_content_manifest(manifest: &Manifest) -> io::Result<()> {
    validate_manifest(manifest)?;
    if let Some(entry) = manifest
        .files
        .iter()
        .find(|entry| !is_base_content_path(&entry.path))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "base-content manifest contains a file outside its ownership boundary: {}",
                entry.path
            ),
        ));
    }
    Ok(())
}

/// Validates a base-content manifest against the release pointer that selected it.
pub(crate) fn validate_base_content_release_manifest(
    manifest: &Manifest,
    version: &str,
) -> io::Result<()> {
    validate_base_content_manifest(manifest)?;
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == "catalog.xml")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base-content release is missing catalog.xml",
        ));
    }
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base-content manifest version does not match its channel pointer",
        ));
    }
    Ok(())
}

/// Identifies files owned by the base-content layer rather than optional packs.
pub(crate) fn is_base_content_path(path: &str) -> bool {
    path == "catalog.xml"
        || path.starts_with("Application/")
        || path.starts_with("Packs/ClassicGalacticCivilWar/")
}
