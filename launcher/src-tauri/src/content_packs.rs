use rebellion2_update_core::{
    apply, apply_with_metadata, diff, replace_file_atomically, store_metadata, validate_manifest,
    validate_manifest_paths, verify_install, BlobSource, Manifest,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as VersionOrdering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::versions::{compare_versions, parse_semantic_version};

pub const CATALOG_PATH: &str = "dist/catalogue.json";
pub const GAME_DEPENDENCY_ID: &str = "rebellion2";
const METADATA_DIRECTORY: &str = ".content-packs";
const INSTALL_RECORD_FILE: &str = "install.json";
const MANIFEST_FILE: &str = "manifest.json";
const PENDING_INSTALL_FILE: &str = "pending-install.json";
const STAGING_DIRECTORY: &str = "staging";

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
    pub minimum_supported_version: Option<String>,
    pub depends: BTreeMap<String, VersionRequirement>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum VersionRequirement {
    Single(String),
    Alternatives(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledContentPack {
    pub id: String,
    pub name: String,
    pub version: String,
    pub directory: String,
}

#[derive(Deserialize)]
#[serde(rename = "ContentPack")]
struct ContentPackIdentity {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Version")]
    version: String,
}

fn default_blobs() -> String {
    "content-pack-blobs/".to_string()
}

pub fn validate_catalog(catalog: &ContentPackCatalog) -> io::Result<()> {
    if catalog.schema_version != 2 {
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
    if pack.name.trim().is_empty() {
        return Err(invalid_data("content-pack name is required"));
    }
    if parse_semantic_version(&pack.version).is_none() {
        return Err(invalid_data(
            "content-pack version must be a semantic version",
        ));
    }
    if pack
        .minimum_supported_version
        .as_deref()
        .is_some_and(|version| {
            parse_semantic_version(version).is_none()
                || compare_versions(version, &pack.version).is_gt()
        })
    {
        return Err(invalid_data(
            "content-pack minimum supported version is invalid",
        ));
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
    if !pack.depends.contains_key(GAME_DEPENDENCY_ID) {
        return Err(invalid_data("content pack must depend on rebellion2"));
    }
    for (id, requirement) in &pack.depends {
        if !is_safe_id(id) || id == &pack.id {
            return Err(invalid_data("content-pack dependency id is invalid"));
        }
        if !requirement.is_valid() {
            return Err(invalid_data(
                "content-pack dependency requirement is invalid",
            ));
        }
    }
    Ok(())
}

/// Returns whether an installed dependency version satisfies a pack's requirement.
pub fn supports_dependency_version(
    pack: &ContentPackRelease,
    dependency_id: &str,
    installed_version: Option<&str>,
) -> bool {
    pack.depends
        .get(dependency_id)
        .zip(installed_version)
        .is_some_and(|(requirement, installed)| requirement.is_satisfied_by(installed))
}

impl VersionRequirement {
    /// Returns the authored requirement text for display.
    pub fn display(&self) -> String {
        match self {
            Self::Single(requirement) => requirement.clone(),
            Self::Alternatives(requirements) => requirements.join(" or "),
        }
    }

    /// Returns whether every alternative uses the supported constraint grammar.
    fn is_valid(&self) -> bool {
        match self {
            Self::Single(requirement) => requirement_expression_is_valid(requirement),
            Self::Alternatives(requirements) => {
                !requirements.is_empty()
                    && requirements
                        .iter()
                        .all(|requirement| requirement_expression_is_valid(requirement))
            }
        }
    }

    /// Returns whether a semantic version satisfies any authored alternative.
    fn is_satisfied_by(&self, version: &str) -> bool {
        if !self.is_valid() || parse_semantic_version(version).is_none() {
            return false;
        }
        match self {
            Self::Single(requirement) => requirement_expression_matches(requirement, version),
            Self::Alternatives(requirements) => requirements
                .iter()
                .any(|requirement| requirement_expression_matches(requirement, version)),
        }
    }
}

/// Returns whether one whitespace-separated conjunction contains valid constraints.
fn requirement_expression_is_valid(requirement: &str) -> bool {
    let mut constraints = requirement.split_whitespace().peekable();
    constraints.peek().is_some()
        && constraints.all(|constraint| parse_version_constraint(constraint).is_some())
}

/// Returns whether every constraint in one alternative matches a version.
fn requirement_expression_matches(requirement: &str, version: &str) -> bool {
    requirement.split_whitespace().all(|constraint| {
        parse_version_constraint(constraint)
            .is_some_and(|(operator, target)| operator.matches(compare_versions(version, target)))
    })
}

/// Parses one exact or ordered semantic-version constraint.
fn parse_version_constraint(constraint: &str) -> Option<(VersionOperator, &str)> {
    let (operator, target) = if let Some(target) = constraint.strip_prefix(">=") {
        (VersionOperator::GreaterOrEqual, target)
    } else if let Some(target) = constraint.strip_prefix("<=") {
        (VersionOperator::LessOrEqual, target)
    } else if let Some(target) = constraint.strip_prefix('>') {
        (VersionOperator::Greater, target)
    } else if let Some(target) = constraint.strip_prefix('<') {
        (VersionOperator::Less, target)
    } else if let Some(target) = constraint.strip_prefix('=') {
        (VersionOperator::Equal, target)
    } else {
        (VersionOperator::Equal, constraint)
    };
    parse_semantic_version(target).map(|_| (operator, target))
}

#[derive(Clone, Copy)]
enum VersionOperator {
    Equal,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl VersionOperator {
    /// Returns whether a semantic-version ordering passes this operator.
    fn matches(self, ordering: VersionOrdering) -> bool {
        match self {
            Self::Equal => ordering == VersionOrdering::Equal,
            Self::Greater => ordering == VersionOrdering::Greater,
            Self::GreaterOrEqual => ordering != VersionOrdering::Less,
            Self::Less => ordering == VersionOrdering::Less,
            Self::LessOrEqual => ordering != VersionOrdering::Greater,
        }
    }
}

pub fn validate_pack_manifest(manifest: &Manifest, pack: &ContentPackRelease) -> io::Result<()> {
    validate_manifest(manifest)?;
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

/// Installs a previously unmanaged pack without exposing partial files in the live pack directory.
pub fn install_fresh(
    install_root: &Path,
    pack: &ContentPackRelease,
    manifest: &Manifest,
    blobs: &dyn BlobSource,
) -> io::Result<usize> {
    validate_release(pack)?;
    validate_pack_manifest(manifest, pack)?;

    let destination = install_directory(install_root, pack)?;
    let pending = read_pending_install(install_root, &pack.id);
    let destination_has_files = directory_has_entries(&destination)?;
    let owns_destination = pending
        .as_ref()
        .is_some_and(|record| record.id == pack.id && record.directory == pack.directory);
    if destination_has_files && !owns_destination {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists but is not managed by the launcher",
                destination.display()
            ),
        ));
    }

    if destination_has_files
        && pending
            .as_ref()
            .is_some_and(|record| record.version == pack.version)
        && verify_install(&destination, manifest)?
    {
        write_install(install_root, pack, manifest)?;
        return Ok(0);
    }

    let metadata = metadata_directory(install_root, &pack.id);
    let staging = metadata.join(STAGING_DIRECTORY);
    remove_directory_if_present(&staging)?;
    fs::create_dir_all(&metadata)?;
    if pending.as_ref().is_none_or(|record| {
        !destination_has_files
            && (record.id != pack.id
                || record.version != pack.version
                || record.directory != pack.directory)
    }) {
        write_json_atomically(&metadata.join(PENDING_INSTALL_FILE), &install_record(pack))?;
    }

    let plan = diff(None, manifest);
    apply(&plan, &staging, blobs)?;
    if !verify_install(&staging, manifest)? {
        return Err(invalid_data(
            "content-pack verification failed after staging",
        ));
    }

    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if destination_has_files {
                fs::remove_dir_all(&destination)?;
            } else {
                fs::remove_dir(&destination)?;
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a directory", destination.display()),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&staging, &destination)?;
    write_install(install_root, pack, manifest)?;
    Ok(plan.changed.len())
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
        .filter(|manifest| validate_manifest(manifest).is_ok())
}

/// Reads a pack definition's version after verifying its declared identity.
pub fn read_pack_version(definition_path: &Path, expected_id: &str) -> Option<String> {
    let xml = fs::read_to_string(definition_path).ok()?;
    let identity: ContentPackIdentity = quick_xml::de::from_str(&xml).ok()?;
    if identity.id.trim() != expected_id {
        return None;
    }
    let version = identity.version.trim();
    (!version.is_empty()).then(|| version.to_string())
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
    let installed = install_record(pack);
    let manifest = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    let installed = serde_json::to_vec(&installed).map_err(io::Error::other)?;
    let manifest_path = format!("{METADATA_DIRECTORY}/{}/{MANIFEST_FILE}", pack.id);
    let install_path = format!("{METADATA_DIRECTORY}/{}/{INSTALL_RECORD_FILE}", pack.id);
    store_metadata(
        install_root,
        &[
            (manifest_path.as_str(), &manifest),
            (install_path.as_str(), &installed),
        ],
    )?;
    clear_pending_install(install_root, &pack.id);
    Ok(())
}

/// Commits an existing pack and its install records under one installation transaction.
pub fn apply_update(
    install_root: &Path,
    pack: &ContentPackRelease,
    manifest: &Manifest,
    plan: &rebellion2_update_core::Plan,
    blobs: &dyn BlobSource,
) -> io::Result<usize> {
    validate_release(pack)?;
    validate_pack_manifest(manifest, pack)?;
    let prefix = format!("Content/Packs/{}/", pack.directory);
    let rooted_plan = rebellion2_update_core::Plan {
        changed: plan
            .changed
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                entry.path = format!("{prefix}{}", entry.path);
                entry
            })
            .collect(),
        removed: plan
            .removed
            .iter()
            .map(|path| format!("{prefix}{path}"))
            .collect(),
    };
    let manifest = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    let installed = serde_json::to_vec(&install_record(pack)).map_err(io::Error::other)?;
    let manifest_path = format!("{METADATA_DIRECTORY}/{}/{MANIFEST_FILE}", pack.id);
    let install_path = format!("{METADATA_DIRECTORY}/{}/{INSTALL_RECORD_FILE}", pack.id);
    apply_with_metadata(
        &rooted_plan,
        install_root,
        blobs,
        &[
            (manifest_path.as_str(), &manifest),
            (install_path.as_str(), &installed),
        ],
    )?;
    clear_pending_install(install_root, &pack.id);
    Ok(plan.changed.len())
}

fn install_record(pack: &ContentPackRelease) -> InstalledContentPack {
    InstalledContentPack {
        id: pack.id.clone(),
        name: pack.name.clone(),
        version: pack.version.clone(),
        directory: pack.directory.clone(),
    }
}

fn read_pending_install(install_root: &Path, id: &str) -> Option<InstalledContentPack> {
    if !is_safe_id(id) {
        return None;
    }
    serde_json::from_slice(
        &fs::read(metadata_directory(install_root, id).join(PENDING_INSTALL_FILE)).ok()?,
    )
    .ok()
}

fn clear_pending_install(install_root: &Path, id: &str) {
    let metadata = metadata_directory(install_root, id);
    let _ = fs::remove_file(metadata.join(PENDING_INSTALL_FILE));
    let _ = remove_directory_if_present(&metadata.join(STAGING_DIRECTORY));
}

fn directory_has_entries(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(fs::read_dir(path)?.next().is_some()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_directory_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn remove_install_metadata(install_root: &Path, id: &str) -> io::Result<()> {
    if !is_safe_id(id) {
        return Err(invalid_data("content-pack id must be a lowercase slug"));
    }
    let directory = metadata_directory(install_root, id);
    remove_directory_if_present(&directory)
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

/// Removes a pack's owned files and its registration in the same recoverable transaction.
pub fn removal_plan(
    installed: &InstalledContentPack,
    manifest: &Manifest,
) -> io::Result<rebellion2_update_core::Plan> {
    if !is_safe_id(&installed.id) || !is_single_path_component(&installed.directory) {
        return Err(invalid_data(
            "content-pack registration contains an unsafe path",
        ));
    }
    let plan = removable_manifest(manifest)?;
    let prefix = format!("Content/Packs/{}/", installed.directory);
    let metadata = format!("{METADATA_DIRECTORY}/{}/", installed.id);
    Ok(rebellion2_update_core::Plan {
        changed: Vec::new(),
        removed: plan
            .removed
            .into_iter()
            .map(|path| format!("{prefix}{path}"))
            .chain(
                [MANIFEST_FILE, INSTALL_RECORD_FILE, PENDING_INSTALL_FILE]
                    .map(|name| format!("{metadata}{name}")),
            )
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
        && (!cfg!(target_os = "windows") || !value.ends_with(['.', ' ']))
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
    replace_file_atomically(&temporary, path)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebellion2_update_core::{sha256_hex, FileEntry};
    use std::collections::BTreeMap;

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
            minimum_supported_version: None,
            depends: BTreeMap::from([
                (
                    GAME_DEPENDENCY_ID.to_string(),
                    VersionRequirement::Single(">=0.0.31".to_string()),
                ),
                (
                    "classic-galactic-civil-war".to_string(),
                    VersionRequirement::Single("1.0.0".to_string()),
                ),
            ]),
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

    struct TestBlobs {
        values: BTreeMap<String, Vec<u8>>,
        fail_on: Option<String>,
    }

    impl BlobSource for TestBlobs {
        fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()> {
            if self.fail_on.as_deref() == Some(entry.sha256.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "interrupted",
                ));
            }
            let bytes = self
                .values
                .get(&entry.sha256)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing blob"))?;
            fs::write(destination, bytes)
        }
    }

    fn install_fixture() -> (Manifest, TestBlobs) {
        let first = b"first".to_vec();
        let second = b"second".to_vec();
        let first_hash = sha256_hex(&first);
        let second_hash = sha256_hex(&second);
        let manifest = Manifest {
            version: "1.2.3".to_string(),
            files: vec![
                FileEntry {
                    path: "pack.xml".to_string(),
                    sha256: first_hash.clone(),
                    size: first.len() as u64,
                },
                FileEntry {
                    path: "Data/ships.xml".to_string(),
                    sha256: second_hash.clone(),
                    size: second.len() as u64,
                },
            ],
        };
        let values = BTreeMap::from([(first_hash, first), (second_hash.clone(), second)]);
        (
            manifest,
            TestBlobs {
                values,
                fail_on: Some(second_hash),
            },
        )
    }

    #[test]
    fn catalog_accepts_independently_scoped_pack() {
        let catalog = ContentPackCatalog {
            schema_version: 2,
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
            schema_version: 2,
            packs: vec![release(), second],
        };

        assert!(validate_catalog(&catalog).is_err());
    }

    #[test]
    fn release_without_game_dependency_is_rejected() {
        let mut pack = release();
        pack.depends.remove(GAME_DEPENDENCY_ID);

        assert!(validate_release(&pack).is_err());
    }

    #[test]
    fn release_with_self_dependency_is_rejected() {
        let mut pack = release();
        pack.depends.insert(
            pack.id.clone(),
            VersionRequirement::Single("1.0.0".to_string()),
        );

        assert!(validate_release(&pack).is_err());
    }

    #[test]
    fn release_with_invalid_dependency_requirement_is_rejected() {
        for requirement in ["", ">=", "^0.0.31", "01.0.0"] {
            let mut pack = release();
            pack.depends.insert(
                GAME_DEPENDENCY_ID.to_string(),
                VersionRequirement::Single(requirement.to_string()),
            );

            assert!(validate_release(&pack).is_err());
        }
    }

    #[test]
    fn release_with_invalid_pack_version_is_rejected() {
        for version in ["next", "01.0.0", "1.0.0-beta.01"] {
            let mut pack = release();
            pack.version = version.to_string();

            assert!(validate_release(&pack).is_err());
        }
    }

    #[test]
    fn release_with_invalid_minimum_supported_version_is_rejected() {
        for version in ["next", "1.2.4"] {
            let mut pack = release();
            pack.minimum_supported_version = Some(version.to_string());

            assert!(validate_release(&pack).is_err());
        }
    }

    #[test]
    fn supports_dependency_version_accepts_a_bounded_range() {
        let mut pack = release();
        pack.depends.insert(
            GAME_DEPENDENCY_ID.to_string(),
            VersionRequirement::Single(">=0.0.31 <0.0.40".to_string()),
        );

        assert!(supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.31")
        ));
        assert!(supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.39")
        ));
        assert!(!supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.40")
        ));
        assert!(!supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            None
        ));
    }

    #[test]
    fn supports_dependency_version_accepts_alternative_ranges() {
        let mut pack = release();
        pack.depends.insert(
            GAME_DEPENDENCY_ID.to_string(),
            VersionRequirement::Alternatives(vec![
                ">=0.0.31 <0.0.35".to_string(),
                ">=0.0.40".to_string(),
            ]),
        );

        assert!(supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.33")
        ));
        assert!(!supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.37")
        ));
        assert!(supports_dependency_version(
            &pack,
            GAME_DEPENDENCY_ID,
            Some("0.0.40")
        ));
    }

    #[test]
    fn release_cannot_escape_content_packs_directory() {
        for directory in ["../Outside", "Nested/Pack", "C:\\Pack"] {
            let mut pack = release();
            pack.directory = directory.to_string();
            assert!(validate_release(&pack).is_err(), "accepted {directory}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn validate_release_with_normalized_directory_alias_rejects_release() {
        for directory in ["SamplePack.", "SamplePack "] {
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

    #[test]
    fn write_json_atomically_with_existing_file_replaces_complete_document() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("install.json");
        fs::write(&path, b"old").unwrap();
        let record = install_record(&release());

        write_json_atomically(&path, &record).unwrap();

        let stored: InstalledContentPack =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.id, record.id);
        assert!(!path.with_extension("part").exists());
    }

    #[test]
    fn interrupted_fresh_install_keeps_live_directory_clean_and_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let (manifest, failing_blobs) = install_fixture();

        assert!(install_fresh(root.path(), &pack, &manifest, &failing_blobs).is_err());
        assert!(!install_directory(root.path(), &pack).unwrap().exists());
        assert!(read_installed(root.path(), &pack.id).is_none());

        let successful_blobs = TestBlobs {
            values: failing_blobs.values,
            fail_on: None,
        };
        assert_eq!(
            install_fresh(root.path(), &pack, &manifest, &successful_blobs).unwrap(),
            2
        );
        assert!(
            verify_install(&install_directory(root.path(), &pack).unwrap(), &manifest).unwrap()
        );
        assert_eq!(
            read_installed(root.path(), &pack.id).unwrap().version,
            pack.version
        );
    }

    #[test]
    fn fresh_install_does_not_replace_an_unmanaged_directory() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let (manifest, blobs) = install_fixture();
        let destination = install_directory(root.path(), &pack).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("custom.txt"), "keep").unwrap();

        let error = install_fresh(root.path(), &pack, &manifest, &blobs).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(destination.join("custom.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn fresh_install_finishes_a_promoted_pending_install_without_downloading_again() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let (manifest, blobs) = install_fixture();
        let destination = install_directory(root.path(), &pack).unwrap();
        let metadata = metadata_directory(root.path(), &pack.id);
        fs::create_dir_all(&metadata).unwrap();
        write_json_atomically(&metadata.join(PENDING_INSTALL_FILE), &install_record(&pack))
            .unwrap();
        let complete_blobs = TestBlobs {
            values: blobs.values,
            fail_on: None,
        };
        apply(&diff(None, &manifest), &destination, &complete_blobs).unwrap();
        let unavailable_blobs = TestBlobs {
            values: BTreeMap::new(),
            fail_on: None,
        };

        assert_eq!(
            install_fresh(root.path(), &pack, &manifest, &unavailable_blobs).unwrap(),
            0
        );
        assert!(read_installed(root.path(), &pack.id).is_some());
        assert!(!metadata.join(PENDING_INSTALL_FILE).exists());
    }

    #[test]
    fn fresh_install_replaces_corrupt_owned_destination_on_retry() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let (manifest, blobs) = install_fixture();
        let destination = install_directory(root.path(), &pack).unwrap();
        let metadata = metadata_directory(root.path(), &pack.id);
        fs::create_dir_all(&metadata).unwrap();
        write_json_atomically(&metadata.join(PENDING_INSTALL_FILE), &install_record(&pack))
            .unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("pack.xml"), b"corrupt").unwrap();
        let complete_blobs = TestBlobs {
            values: blobs.values,
            fail_on: None,
        };

        assert_eq!(
            install_fresh(root.path(), &pack, &manifest, &complete_blobs).unwrap(),
            2
        );
        assert!(verify_install(&destination, &manifest).unwrap());
        assert!(read_installed(root.path(), &pack.id).is_some());
    }

    #[test]
    fn fresh_install_replaces_empty_destination_directory() {
        let root = tempfile::tempdir().unwrap();
        let pack = release();
        let (manifest, blobs) = install_fixture();
        let destination = install_directory(root.path(), &pack).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let complete_blobs = TestBlobs {
            values: blobs.values,
            fail_on: None,
        };

        assert_eq!(
            install_fresh(root.path(), &pack, &manifest, &complete_blobs).unwrap(),
            2
        );
        assert!(verify_install(&destination, &manifest).unwrap());
    }

    #[test]
    fn read_pack_version_requires_matching_identity() {
        let root = tempfile::tempdir().unwrap();
        let definition = root.path().join("pack.xml");
        fs::write(
            &definition,
            r#"<?xml version="1.0" encoding="utf-8"?>
<ContentPack>
  <ID>base-pack</ID>
  <Version>1.0.0</Version>
  <Preloads>
    <Preload>
      <ID>main-menu</ID>
      <Path>Preload/main-menu.xml</Path>
    </Preload>
    <Preload>
      <ID>strategy</ID>
      <Path>Preload/strategy.xml</Path>
    </Preload>
  </Preloads>
</ContentPack>"#,
        )
        .unwrap();

        assert_eq!(
            read_pack_version(&definition, "base-pack").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(read_pack_version(&definition, "other-pack"), None);
    }

    #[test]
    fn read_pack_version_rejects_nested_identity_without_root_identity() {
        let root = tempfile::tempdir().unwrap();
        let definition = root.path().join("pack.xml");
        fs::write(
            &definition,
            "<ContentPack><Preloads><Preload><ID>base-pack</ID><Version>1.0.0</Version></Preload></Preloads></ContentPack>",
        )
        .unwrap();

        assert_eq!(read_pack_version(&definition, "base-pack"), None);
    }
}
