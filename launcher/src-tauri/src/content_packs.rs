use rebellion2_update_core::{validate_manifest_paths, Manifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const CATALOG_PATH: &str = "dist/content-packs.json";
const METADATA_DIRECTORY: &str = ".content-packs";
const INSTALL_RECORD_FILE: &str = "install.json";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPackCatalog {
    pub schema_version: u32,
    pub packs: Vec<ContentPackRelease>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPackRelease {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: String,
    pub directory: String,
    pub manifest: String,
    #[serde(default = "default_blobs")]
    pub blobs: String,
    #[serde(default)]
    pub download_size: u64,
    #[serde(default)]
    pub base_pack_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledContentPack {
    pub id: String,
    pub name: String,
    pub version: String,
    pub directory: String,
}

fn default_blobs() -> String {
    "content-pack-blobs/".to_string()
}

pub fn validate_catalog(catalog: &ContentPackCatalog) -> io::Result<()> {
    if catalog.schema_version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported content-pack catalog schema {}",
                catalog.schema_version
            ),
        ));
    }

    let mut ids = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for pack in &catalog.packs {
        validate_release(pack)?;
        if !ids.insert(pack.id.to_ascii_lowercase()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate content-pack id: {}", pack.id),
            ));
        }
        if !directories.insert(pack.directory.to_ascii_lowercase()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate content-pack directory: {}", pack.directory),
            ));
        }
    }
    Ok(())
}

pub fn validate_release(pack: &ContentPackRelease) -> io::Result<()> {
    if !is_safe_id(&pack.id) {
        return Err(invalid_data("content-pack id must be a lowercase slug"));
    }
    if pack.name.trim().is_empty() || pack.version.trim().is_empty() {
        return Err(invalid_data("content-pack name and version are required"));
    }
    if !is_single_path_component(&pack.directory) {
        return Err(invalid_data(
            "content-pack directory must be one safe path component",
        ));
    }
    validate_channel_path(&pack.manifest)?;
    validate_channel_path(&pack.blobs)?;
    if !pack.blobs.ends_with('/') {
        return Err(invalid_data("content-pack blob path must end with a slash"));
    }
    if let Some(base_pack_id) = &pack.base_pack_id {
        if !is_safe_id(base_pack_id) {
            return Err(invalid_data(
                "base content-pack id must be a lowercase slug",
            ));
        }
        if base_pack_id == &pack.id {
            return Err(invalid_data("a content pack cannot inherit from itself"));
        }
    }
    Ok(())
}

pub fn validate_pack_manifest(manifest: &Manifest, pack: &ContentPackRelease) -> io::Result<()> {
    validate_manifest_paths(manifest)?;
    if manifest.version != pack.version {
        return Err(invalid_data(
            "content-pack manifest version does not match the catalog",
        ));
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path.eq_ignore_ascii_case("pack.xml"))
    {
        return Err(invalid_data(
            "content-pack manifest does not contain pack.xml",
        ));
    }
    Ok(())
}

pub fn install_directory(install_root: &Path, pack: &ContentPackRelease) -> io::Result<PathBuf> {
    validate_release(pack)?;
    Ok(install_root
        .join("Content")
        .join("Packs")
        .join(&pack.directory))
}

pub fn read_installed(install_root: &Path, id: &str) -> Option<InstalledContentPack> {
    if !is_safe_id(id) {
        return None;
    }
    let installed: InstalledContentPack = serde_json::from_slice(
        &fs::read(metadata_directory(install_root, id).join(INSTALL_RECORD_FILE)).ok()?,
    )
    .ok()?;
    (installed.id == id
        && is_safe_id(&installed.id)
        && is_single_path_component(&installed.directory)
        && !installed.name.trim().is_empty()
        && !installed.version.trim().is_empty())
    .then_some(installed)
}

pub fn read_manifest(install_root: &Path, id: &str) -> Option<Manifest> {
    if !is_safe_id(id) {
        return None;
    }
    Manifest::from_json(&fs::read(metadata_directory(install_root, id).join(MANIFEST_FILE)).ok()?)
        .ok()
}

pub fn write_install(
    install_root: &Path,
    pack: &ContentPackRelease,
    manifest: &Manifest,
) -> io::Result<()> {
    validate_release(pack)?;
    validate_pack_manifest(manifest, pack)?;
    let metadata = metadata_directory(install_root, &pack.id);
    fs::create_dir_all(&metadata)?;
    let installed = InstalledContentPack {
        id: pack.id.clone(),
        name: pack.name.clone(),
        version: pack.version.clone(),
        directory: pack.directory.clone(),
    };
    write_json_atomically(&metadata.join(MANIFEST_FILE), manifest)?;
    write_json_atomically(&metadata.join(INSTALL_RECORD_FILE), &installed)
}

pub fn remove_install_metadata(install_root: &Path, id: &str) -> io::Result<()> {
    if !is_safe_id(id) {
        return Err(invalid_data("content-pack id must be a lowercase slug"));
    }
    let directory = metadata_directory(install_root, id);
    if directory.exists() {
        fs::remove_dir_all(directory)?;
    }
    Ok(())
}

pub fn removable_manifest(manifest: &Manifest) -> io::Result<rebellion2_update_core::Plan> {
    validate_manifest_paths(manifest)?;
    Ok(rebellion2_update_core::Plan {
        changed: Vec::new(),
        removed: manifest
            .files
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
    })
}

fn metadata_directory(install_root: &Path, id: &str) -> PathBuf {
    install_root.join(METADATA_DIRECTORY).join(id)
}

fn is_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        })
}

fn is_single_path_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains(':')
}

fn validate_channel_path(value: &str) -> io::Result<()> {
    let path = value.trim_end_matches('/');
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', ':'])
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(invalid_data("content-pack channel path is unsafe"));
    }
    Ok(())
}

fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("part");
    let bytes = serde_json::to_vec(value).map_err(|error| io::Error::other(error.to_string()))?;
    fs::write(&temporary, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebellion2_update_core::FileEntry;

    fn release() -> ContentPackRelease {
        ContentPackRelease {
            id: "sample-pack".to_string(),
            name: "Sample Pack".to_string(),
            description: String::new(),
            version: "1.2.3".to_string(),
            directory: "SamplePack".to_string(),
            manifest: "dist/content-packs/sample-pack/manifest-1.2.3.json".to_string(),
            blobs: "content-pack-blobs/".to_string(),
            download_size: 4,
            base_pack_id: Some("classic-galactic-civil-war".to_string()),
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            version: "1.2.3".to_string(),
            files: vec![FileEntry {
                path: "pack.xml".to_string(),
                sha256: "00".repeat(32),
                size: 4,
            }],
        }
    }

    #[test]
    fn catalog_accepts_independently_scoped_pack() {
        let catalog = ContentPackCatalog {
            schema_version: 1,
            packs: vec![release()],
        };

        validate_catalog(&catalog).unwrap();
        validate_pack_manifest(&manifest(), &catalog.packs[0]).unwrap();
    }

    #[test]
    fn catalog_rejects_duplicate_install_directories() {
        let mut second = release();
        second.id = "another-pack".to_string();
        let catalog = ContentPackCatalog {
            schema_version: 1,
            packs: vec![release(), second],
        };

        assert!(validate_catalog(&catalog).is_err());
    }

    #[test]
    fn release_cannot_escape_content_packs_directory() {
        for directory in ["../Outside", "Nested/Pack", "C:\\Pack"] {
            let mut pack = release();
            pack.directory = directory.to_string();
            assert!(validate_release(&pack).is_err(), "accepted {directory}");
        }
    }

    #[test]
    fn release_requires_canonical_slug_id() {
        for id in ["-sample", "sample-", "sample--pack", "SamplePack"] {
            let mut pack = release();
            pack.id = id.to_string();
            assert!(validate_release(&pack).is_err(), "accepted {id}");
        }
    }

    #[test]
    fn install_metadata_round_trips_outside_content_tree() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let manifest = manifest();

        write_install(root.path(), &pack, &manifest).unwrap();

        let installed = read_installed(root.path(), &pack.id).unwrap();
        assert_eq!(installed.version, pack.version);
        assert_eq!(read_manifest(root.path(), &pack.id).unwrap(), manifest);
        assert!(!root.path().join("Content").exists());
    }
}
