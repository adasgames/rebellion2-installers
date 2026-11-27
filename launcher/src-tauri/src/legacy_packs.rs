use rebellion2_update_core::{validate_manifest, Manifest};
use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Registration {
    id: String,
    name: String,
    version: String,
    directory: String,
}

#[derive(Deserialize)]
#[serde(rename = "ContentPack")]
struct Identity {
    #[serde(rename = "ID")]
    id: String,
}

pub struct RetiredPack {
    pub name: String,
    pub backup: PathBuf,
}

/// Backs up registered patcher installations and retires their registrations once.
/// Directory renaming preserves every file, including edits and untracked additions.
/// A remaining registration lets a later run finish cleanup after an interrupted rename.
pub fn retire(install_root: &Path) -> io::Result<Vec<RetiredPack>> {
    let metadata = install_root.join(".content-packs");
    if !real_directory(&metadata)? {
        return Ok(Vec::new());
    }
    let mut retired = Vec::new();
    for entry in fs::read_dir(&metadata)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let record = entry.path().join("install.json");
        if !ordinary_file(&record)? {
            continue;
        }
        let registration: Registration = serde_json::from_slice(&fs::read(&record)?)?;
        if registration.id != entry.file_name().to_string_lossy()
            || !safe_id(&registration.id)
            || !safe_component(&registration.directory)
            || registration.name.trim().is_empty()
            || registration.version.trim().is_empty()
        {
            return Err(invalid(
                "The optional pack registration contains an invalid identity or folder.",
            ));
        }
        let manifest_path = entry.path().join("manifest.json");
        if !ordinary_file(&manifest_path)? {
            return Err(invalid(
                "The optional pack's installation manifest is missing.",
            ));
        }
        let manifest = Manifest::from_json(&fs::read(manifest_path)?)?;
        validate_manifest(&manifest)?;
        if manifest.version != registration.version
            || !manifest.files.iter().any(|file| file.path == "pack.xml")
        {
            return Err(invalid(
                "The optional pack's registration and manifest do not match.",
            ));
        }
        let content = install_root.join("Content");
        let packs = content.join("Packs");
        real_directory(&content)?;
        real_directory(&packs)?;
        let base_manifest_path = content.join(".manifest.json");
        if ordinary_file(&base_manifest_path)? {
            let base_manifest = Manifest::from_json(&fs::read(base_manifest_path)?)?;
            validate_manifest(&base_manifest)?;
            let prefix = format!("Packs/{}/", registration.directory).to_ascii_lowercase();
            if base_manifest
                .files
                .iter()
                .any(|file| file.path.to_ascii_lowercase().starts_with(&prefix))
            {
                return Err(invalid(
                    "The registered folder is owned by bundled game content and cannot be removed.",
                ));
            }
        }
        let source = packs.join(&registration.directory);
        let retired_root = entry.path().join("retired");
        let backup = retired_root.join("pack");
        real_directory(&retired_root)?;
        let source_exists = real_directory(&source)?;
        let backup_exists = real_directory(&backup)?;
        if source_exists {
            check_identity(&source, &registration.id)?;
            if backup_exists {
                return Err(invalid(
                    "Both an old pack and its backup exist. Neither has been overwritten.",
                ));
            }
            fs::create_dir_all(&retired_root)?;
            fs::rename(&source, &backup)?;
        } else if backup_exists {
            check_identity(&backup, &registration.id)?;
        } else {
            fs::remove_file(record)?;
            continue;
        }
        fs::remove_file(record)?;
        retired.push(RetiredPack {
            name: registration.name,
            backup,
        });
    }
    Ok(retired)
}

/// Validates the pack identity before renaming its directory or completing prior cleanup.
fn check_identity(folder: &Path, expected: &str) -> io::Result<()> {
    let path = folder.join("pack.xml");
    if !ordinary_file(&path)? {
        return Err(invalid("The registered optional pack has no pack.xml."));
    }
    let identity: Identity =
        quick_xml::de::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
    if identity.id != expected {
        return Err(invalid(
            "The installed pack does not match its registration.",
        ));
    }
    Ok(())
}

/// Accepts the lowercase identifiers used by existing installation records.
fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// Accepts direct-child folder names supported by existing installation records.
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', ':', '\0'])
        && (!cfg!(target_os = "windows") || !value.ends_with(['.', ' ']))
}

/// Checks directories without following links to another installation or volume.
fn real_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(invalid(
            "An optional pack path is not an ordinary directory.",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Checks metadata files without following symbolic links.
fn ordinary_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(invalid("An optional pack record is not an ordinary file.")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebellion2_update_core::{sha256_hex, FileEntry};

    fn fixture(root: &Path) -> PathBuf {
        let pack = root.join("Content/Packs/Example");
        let metadata = root.join(".content-packs/example");
        fs::create_dir_all(&pack).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        let xml = b"<ContentPack><ID>example</ID><Version>1.0.0</Version></ContentPack>";
        fs::write(pack.join("pack.xml"), xml).unwrap();
        fs::write(pack.join("edited.txt"), "local edits").unwrap();
        fs::write(
            metadata.join("install.json"),
            br#"{"id":"example","name":"Example","version":"1.0.0","directory":"Example"}"#,
        )
        .unwrap();
        let manifest = Manifest {
            version: "1.0.0".into(),
            files: vec![FileEntry {
                path: "pack.xml".into(),
                sha256: sha256_hex(xml),
                size: xml.len() as u64,
            }],
        };
        fs::write(
            metadata.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        pack
    }

    #[test]
    fn retire_preserves_all_files_and_runs_only_once() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        let result = retire(root.path()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            fs::read_to_string(result[0].backup.join("edited.txt")).unwrap(),
            "local edits"
        );
        assert!(!source.exists());
        assert!(!root
            .path()
            .join(".content-packs/example/install.json")
            .exists());
        assert!(retire(root.path()).unwrap().is_empty());
    }

    #[test]
    fn retire_leaves_thunderstore_and_unregistered_packs_alone() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        for path in [
            "Mods/example/pack.xml",
            "Content/Packs/Unregistered/pack.xml",
        ] {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "preserved").unwrap();
        }
        retire(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("Mods/example/pack.xml")).unwrap(),
            "preserved"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("Content/Packs/Unregistered/pack.xml")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn retire_finishes_after_interrupted_directory_rename() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        let backup = root.path().join(".content-packs/example/retired/pack");
        fs::create_dir_all(backup.parent().unwrap()).unwrap();
        fs::rename(source, &backup).unwrap();
        assert_eq!(retire(root.path()).unwrap().len(), 1);
        assert!(backup.join("edited.txt").exists());
        assert!(retire(root.path()).unwrap().is_empty());
    }

    #[test]
    fn retire_existing_backup_preserves_both_copies() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        let backup = root.path().join(".content-packs/example/retired/pack");
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("keep"), "backup").unwrap();
        assert!(retire(root.path()).is_err());
        assert!(source.join("pack.xml").exists());
        assert_eq!(fs::read_to_string(backup.join("keep")).unwrap(), "backup");
    }

    #[test]
    fn retire_never_removes_folders_owned_by_base_content() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        let xml = fs::read(source.join("pack.xml")).unwrap();
        let manifest = Manifest {
            version: "2.0.0".into(),
            files: vec![FileEntry {
                path: "Packs/Example/pack.xml".into(),
                sha256: sha256_hex(&xml),
                size: xml.len() as u64,
            }],
        };
        fs::write(
            root.path().join("Content/.manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(retire(root.path()).is_err());
        assert!(source.join("pack.xml").exists());
    }

    #[test]
    fn retire_mismatched_pack_identity_preserves_installation() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        fs::write(
            source.join("pack.xml"),
            "<ContentPack><ID>another-pack</ID></ContentPack>",
        )
        .unwrap();
        assert!(retire(root.path()).is_err());
        assert!(source.join("edited.txt").exists());
    }

    #[test]
    fn retire_preserves_supported_folder_names_with_spaces() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        fs::rename(&source, source.with_file_name("Example pack")).unwrap();
        fs::write(
            root.path().join(".content-packs/example/install.json"),
            br#"{"id":"example","name":"Example","version":"1.0.0","directory":"Example pack"}"#,
        )
        .unwrap();
        assert_eq!(retire(root.path()).unwrap().len(), 1);
    }

    #[test]
    fn retire_unsafe_registration_cannot_escape_pack_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        fs::write(
            root.path().join(".content-packs/example/install.json"),
            br#"{"id":"example","name":"Example","version":"1.0.0","directory":"../../Mods"}"#,
        )
        .unwrap();
        assert!(retire(root.path()).is_err());
        assert!(source.exists());
    }

    #[cfg(unix)]
    #[test]
    fn retire_linked_pack_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path());
        let elsewhere = root.path().join("Elsewhere");
        fs::rename(&source, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &source).unwrap();
        assert!(retire(root.path()).is_err());
        assert!(elsewhere.join("edited.txt").exists());
    }
}
