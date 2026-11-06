#[cfg(target_os = "macos")]
use crate::MACOS_GAME_ARCHIVE_FILE_NAME;
use crate::{CONTENT_MANIFEST_FILE, LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME};
use rebellion2_update_core::{validate_manifest, verify_install, FileEntry, Manifest};
use std::path::{Component, Path};
use std::{fs, io};

const BASE_CONTENT_DEFINITIONS: [&str; 2] =
    ["catalog.xml", "Packs/ClassicGalacticCivilWar/pack.xml"];

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
    for required in BASE_CONTENT_DEFINITIONS {
        if !manifest.files.iter().any(|entry| entry.path == required) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("base-content release is missing {required}"),
            ));
        }
    }
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base-content manifest version does not match its channel pointer",
        ));
    }
    Ok(())
}

/// Rechecks the small startup definitions while retaining cached verification for bulk assets.
pub(crate) fn verify_base_content_definitions(
    content_dir: &Path,
    manifest: &Manifest,
) -> io::Result<bool> {
    validate_base_content_release_manifest(manifest, &manifest.version)?;
    let definitions = Manifest {
        version: manifest.version.clone(),
        files: manifest
            .files
            .iter()
            .filter(|entry| BASE_CONTENT_DEFINITIONS.contains(&entry.path.as_str()))
            .cloned()
            .collect(),
    };
    verify_install(content_dir, &definitions)
}

/// Verifies startup definitions against local metadata before launching, including offline launches.
/// Older installations without a manifest must still contain readable startup definitions.
pub(crate) fn verify_installed_content_definitions(
    content_dir: &Path,
    version: &str,
) -> io::Result<bool> {
    let bytes = match fs::read(content_dir.join(CONTENT_MANIFEST_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            for required in BASE_CONTENT_DEFINITIONS {
                match fs::read(content_dir.join(required)) {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    let manifest = Manifest::from_json(&bytes).map_err(io::Error::other)?;
    validate_base_content_release_manifest(&manifest, version)?;
    verify_base_content_definitions(content_dir, &manifest)
}

/// Identifies files owned by the base-content layer rather than optional packs.
pub(crate) fn is_base_content_path(path: &str) -> bool {
    path == "catalog.xml"
        || path.starts_with("Application/")
        || path.starts_with("Packs/ClassicGalacticCivilWar/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebellion2_update_core::sha256_hex;

    /// Creates a complete startup-definition inventory and its installed metadata.
    fn installed_definitions() -> (tempfile::TempDir, Manifest) {
        let directory = tempfile::tempdir().unwrap();
        let files = BASE_CONTENT_DEFINITIONS
            .iter()
            .map(|path| {
                let bytes = format!("definition for {path}").into_bytes();
                let destination = directory.path().join(path);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::write(destination, &bytes).unwrap();
                FileEntry {
                    path: path.to_string(),
                    sha256: sha256_hex(&bytes),
                    size: bytes.len() as u64,
                }
            })
            .collect();
        let manifest = Manifest {
            version: "0.1.3".to_string(),
            files,
        };
        fs::write(
            directory.path().join(CONTENT_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        (directory, manifest)
    }

    #[test]
    fn validate_base_content_release_manifest_missing_pack_definition_returns_error() {
        let (_, mut manifest) = installed_definitions();
        manifest.files.retain(|entry| entry.path == "catalog.xml");

        assert!(validate_base_content_release_manifest(&manifest, "0.1.3").is_err());
    }

    #[test]
    fn verify_installed_content_definitions_matching_files_returns_true() {
        let (directory, _) = installed_definitions();

        assert!(verify_installed_content_definitions(directory.path(), "0.1.3").unwrap());
    }

    #[test]
    fn verify_base_content_definitions_deleted_pack_returns_false() {
        let (directory, manifest) = installed_definitions();
        fs::remove_file(directory.path().join(BASE_CONTENT_DEFINITIONS[1])).unwrap();

        assert!(!verify_base_content_definitions(directory.path(), &manifest).unwrap());
    }

    #[test]
    fn verify_installed_content_definitions_same_size_damage_returns_false() {
        let (directory, manifest) = installed_definitions();
        let pack = &manifest.files[1];
        fs::write(
            directory.path().join(&pack.path),
            vec![b'x'; pack.size as usize],
        )
        .unwrap();

        assert!(!verify_installed_content_definitions(directory.path(), "0.1.3").unwrap());
    }

    #[test]
    fn verify_base_content_definitions_bulk_assets_are_not_reread() {
        let (directory, mut manifest) = installed_definitions();
        manifest.files.push(FileEntry {
            path: "Application/large.bin".to_string(),
            sha256: "0".repeat(64),
            size: 1024 * 1024 * 1024,
        });

        assert!(verify_base_content_definitions(directory.path(), &manifest).unwrap());
    }

    #[test]
    fn verify_installed_content_definitions_different_version_returns_error() {
        let (directory, _) = installed_definitions();

        assert!(verify_installed_content_definitions(directory.path(), "0.1.4").is_err());
    }

    #[test]
    fn verify_installed_content_definitions_malformed_metadata_returns_error() {
        let (directory, _) = installed_definitions();
        fs::write(directory.path().join(CONTENT_MANIFEST_FILE), b"{").unwrap();

        assert!(verify_installed_content_definitions(directory.path(), "0.1.3").is_err());
    }

    #[test]
    fn verify_installed_content_definitions_legacy_without_metadata_returns_true() {
        let (directory, _) = installed_definitions();
        fs::remove_file(directory.path().join(CONTENT_MANIFEST_FILE)).unwrap();

        assert!(verify_installed_content_definitions(directory.path(), "0.1.3").unwrap());
    }

    #[test]
    fn verify_installed_content_definitions_legacy_missing_pack_returns_false() {
        let (directory, _) = installed_definitions();
        fs::remove_file(directory.path().join(CONTENT_MANIFEST_FILE)).unwrap();
        fs::remove_file(directory.path().join(BASE_CONTENT_DEFINITIONS[1])).unwrap();

        assert!(!verify_installed_content_definitions(directory.path(), "0.1.3").unwrap());
    }
}
