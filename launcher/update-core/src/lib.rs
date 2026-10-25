//! Content update engine for the Rebellion II launcher.
//!
//! Pure logic, no Tauri and no network: the launcher shell injects a
//! [`BlobSource`] (remote object storage, a local dir, whatever) and this crate does
//! the manifest diff, hash-verified apply, and integrity check. Keeping it
//! separate makes the risky part — "did we patch the install correctly?" —
//! unit-testable without building the whole desktop app.
//!
//! The manifest format is exactly what `content/gen-manifest.py` emits:
//! `{"version": "...", "files": [{"path", "sha256", "size"}]}`.

mod installation;
mod metadata;

pub use installation::InstallationLock;
pub use metadata::{apply_verified_with_metadata, apply_with_metadata, store_metadata};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::Path;

const TRANSACTION_DIRECTORY: &str = ".rebellion2-update-transaction";
const TRANSACTION_RECORD_FILE: &str = "transaction.json";
const TRANSACTION_RECORD_TEMP_FILE: &str = "transaction.json.part";
const STAGED_DIRECTORY: &str = "staged";
const BACKUP_DIRECTORY: &str = "backup";
const ROLLBACK_DIRECTORY: &str = "rollback";

/// One file in a content manifest: its install-relative POSIX path, the SHA-256
/// of its bytes (also its blob key), and its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// A published content version: a flat list of every file it contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    /// Parse the JSON produced by `gen-manifest.py`.
    pub fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    fn by_path(&self) -> BTreeMap<String, &FileEntry> {
        self.files
            .iter()
            .map(|file| (manifest_path_key(&file.path), file))
            .collect()
    }
}

/// Rejects manifest paths that could escape the directory owned by an update layer.
/// Update manifests always use non-empty, relative POSIX paths.
pub fn validate_manifest_paths(manifest: &Manifest) -> io::Result<()> {
    let mut paths = BTreeSet::new();
    for entry in &manifest.files {
        if !is_safe_manifest_path(&entry.path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest contains an unsafe path: {}", entry.path),
            ));
        }
        let key = manifest_path_key(&entry.path);
        if !paths.insert(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest contains a duplicate path: {}", entry.path),
            ));
        }
    }
    Ok(())
}

/// Rejects malformed manifest entries before their hashes are used as blob or cache keys.
pub fn validate_manifest(manifest: &Manifest) -> io::Result<()> {
    validate_manifest_paths(manifest)?;
    for entry in &manifest.files {
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest contains an invalid SHA-256 for {}", entry.path),
            ));
        }
    }
    Ok(())
}

/// Match manifest paths using the rules of the platform receiving the update.
fn manifest_path_key(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.to_owned()
    }
}

/// The set of operations to turn a local install into the target version:
/// files to (re)write, and files to delete.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub changed: Vec<FileEntry>,
    pub removed: Vec<String>,
}

impl Plan {
    /// True when the install already matches the target — nothing to fetch.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }

    /// Total bytes that must be downloaded to apply this plan.
    pub fn download_size(&self) -> u64 {
        self.changed
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.size))
    }
}

/// Compute what must change to go from `local` (`None` = fresh install) to
/// `remote`. A file is "changed" when it's new or its hash differs; a file is
/// "removed" when the local install has it but the target no longer does.
pub fn diff(local: Option<&Manifest>, remote: &Manifest) -> Plan {
    let current = local.map(Manifest::by_path).unwrap_or_default();
    let target = remote.by_path();

    let mut changed = Vec::new();
    for file in &remote.files {
        match current.get(&manifest_path_key(&file.path)) {
            Some(existing) if existing.sha256 == file.sha256 => {}
            _ => changed.push(file.clone()),
        }
    }

    let mut removed: Vec<String> = current
        .iter()
        .filter(|(path, _)| !target.contains_key(*path))
        .map(|(_, entry)| entry.path.clone())
        .collect();
    removed.sort();

    Plan { changed, removed }
}

/// Includes damaged or missing cached files when updating an installation that is being verified.
pub fn diff_verified(
    local: Option<&Manifest>,
    remote: &Manifest,
    directory: &Path,
) -> io::Result<Plan> {
    validate_manifest(remote)?;
    if let Some(local) = local {
        validate_manifest(local)?;
    }
    let mut plan = diff(local, remote);
    let changed: BTreeSet<String> = plan
        .changed
        .iter()
        .map(|entry| manifest_path_key(&entry.path))
        .collect();
    for entry in &remote.files {
        if !changed.contains(&manifest_path_key(&entry.path)) && !verify_file(directory, entry)? {
            plan.changed.push(entry.clone());
        }
    }
    Ok(plan)
}

/// Hex SHA-256 of a byte slice (matches the blob keys / manifest hashes).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Hashes a file using a fixed-size buffer instead of loading assets into memory.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Where blob bytes come from, keyed by their SHA-256. The launcher supplies an
/// HTTP-backed impl; tests supply an in-memory or local-dir impl.
pub trait BlobSource {
    /// Stages one blob without requiring its complete contents in memory.
    fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()>;
}

#[derive(Debug, Serialize, Deserialize)]
struct TransactionRecord {
    plan: Plan,
    original_files: Vec<String>,
}

/// Applies a plan under `install_dir` without exposing partially downloaded files.
/// Every changed file is downloaded, hash-checked, and staged before the live
/// installation is touched. Original files are retained until the commit completes,
/// allowing a later launcher run to roll back an interrupted commit.
///
/// Returns the number of files written.
pub fn apply(plan: &Plan, install_dir: &Path, blobs: &dyn BlobSource) -> io::Result<usize> {
    apply_checked(plan, install_dir, blobs, || Ok(()))
}

/// Applies a plan and verifies the complete resulting inventory before discarding rollback data.
pub fn apply_verified(
    plan: &Plan,
    install_dir: &Path,
    blobs: &dyn BlobSource,
    expected: &Manifest,
) -> io::Result<usize> {
    validate_manifest(expected)?;
    apply_checked(plan, install_dir, blobs, || {
        if verify_install(install_dir, expected)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "installed files do not match the expected manifest",
            ))
        }
    })
}

/// Keeps the transaction recoverable until both the commit and caller's verification succeed.
fn apply_checked(
    plan: &Plan,
    install_dir: &Path,
    blobs: &dyn BlobSource,
    verify: impl FnOnce() -> io::Result<()>,
) -> io::Result<usize> {
    validate_plan_paths(plan)?;
    recover_interrupted_apply(install_dir)?;
    if plan.is_empty() {
        verify()?;
        return Ok(0);
    }

    let transaction = install_dir.join(TRANSACTION_DIRECTORY);
    remove_directory_if_present(&transaction)?;
    let staged = transaction.join(STAGED_DIRECTORY);
    fs::create_dir_all(&staged)?;

    for entry in &plan.changed {
        let destination = staged.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        blobs.fetch_to(entry, &destination)?;
        let size = fs::metadata(&destination)?.len();
        if size != entry.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "size mismatch for {}: expected {}, got {}",
                    entry.path, entry.size, size
                ),
            ));
        }
        let actual = sha256_file(&destination)?;
        if actual != entry.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "hash mismatch for {}: expected {}, got {actual}",
                    entry.path, entry.sha256
                ),
            ));
        }
    }

    let changed_paths: BTreeSet<String> = plan
        .changed
        .iter()
        .map(|entry| manifest_path_key(&entry.path))
        .collect();
    let effective_removed: Vec<&String> = plan
        .removed
        .iter()
        .filter(|path| !changed_paths.contains(&manifest_path_key(path)))
        .collect();
    let mut touched_paths: Vec<&str> = plan
        .changed
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    touched_paths.extend(effective_removed.iter().map(|path| path.as_str()));

    let backup = transaction.join(BACKUP_DIRECTORY);
    let mut original_files = Vec::new();
    for path in touched_paths {
        validate_managed_path(install_dir, path)?;
        let source = install_dir.join(path);
        match fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.is_file() => {
                let destination = backup.join(path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(source, destination)?;
                original_files.push(path.to_string());
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("update destination is not a file: {path}"),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    let record = TransactionRecord {
        plan: Plan {
            changed: plan.changed.clone(),
            removed: effective_removed.into_iter().cloned().collect(),
        },
        original_files,
    };
    write_transaction_record(&transaction, &record)?;

    if let Err(commit_error) =
        commit_transaction(install_dir, &transaction, &record.plan).and_then(|()| verify())
    {
        return match rollback_transaction(install_dir, &transaction, &record) {
            Ok(()) => Err(commit_error),
            Err(rollback_error) => Err(io::Error::other(format!(
                "{commit_error}; restoring the previous installation also failed: {rollback_error}"
            ))),
        };
    }

    fs::remove_file(transaction.join(TRANSACTION_RECORD_FILE))?;
    let _ = remove_directory_if_present(&transaction);

    Ok(plan.changed.len())
}

/// Restores the previous installation when an earlier apply was interrupted during commit.
/// Returns `true` when recovery work was found.
pub fn recover_interrupted_apply(install_dir: &Path) -> io::Result<bool> {
    let transaction = install_dir.join(TRANSACTION_DIRECTORY);
    match fs::symlink_metadata(&transaction) {
        Ok(metadata) if metadata_is_plain_directory(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update transaction path is not a directory",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }

    let record_path = transaction.join(TRANSACTION_RECORD_FILE);
    match fs::symlink_metadata(&record_path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update transaction record is not a file",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            remove_directory_if_present(&transaction)?;
            return Ok(true);
        }
        Err(error) => return Err(error),
    }

    let record: TransactionRecord = serde_json::from_slice(&fs::read(record_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    validate_plan_paths(&record.plan)?;
    validate_original_paths(&record)?;
    rollback_transaction(install_dir, &transaction, &record)?;
    Ok(true)
}

fn write_transaction_record(transaction: &Path, record: &TransactionRecord) -> io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| io::Error::other(error.to_string()))?;
    let temporary = transaction.join(TRANSACTION_RECORD_TEMP_FILE);
    fs::write(&temporary, bytes)?;
    replace_file_atomically(&temporary, &transaction.join(TRANSACTION_RECORD_FILE))
}

fn commit_transaction(install_dir: &Path, transaction: &Path, plan: &Plan) -> io::Result<()> {
    for entry in &plan.changed {
        validate_managed_path(install_dir, &entry.path)?;
        let source = transaction.join(STAGED_DIRECTORY).join(&entry.path);
        let destination = install_dir.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        replace_file_atomically(&source, &destination)?;
    }

    for path in &plan.removed {
        validate_managed_path(install_dir, path)?;
        remove_file_if_present(&install_dir.join(path))?;
    }
    Ok(())
}

fn rollback_transaction(
    install_dir: &Path,
    transaction: &Path,
    record: &TransactionRecord,
) -> io::Result<()> {
    let original_paths: BTreeSet<String> = record
        .original_files
        .iter()
        .map(|path| manifest_path_key(path))
        .collect();
    let mut touched_paths: Vec<&str> = record
        .plan
        .changed
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    touched_paths.extend(record.plan.removed.iter().map(String::as_str));

    for path in touched_paths {
        validate_managed_path(install_dir, path)?;
        let destination = install_dir.join(path);
        if original_paths.contains(&manifest_path_key(path)) {
            let backup = transaction.join(BACKUP_DIRECTORY);
            if !fs::symlink_metadata(&backup)
                .is_ok_and(|metadata| metadata_is_plain_directory(&metadata))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "update backup path is not a directory",
                ));
            }
            let source = backup.join(path);
            validate_managed_path(&backup, path)?;
            match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.file_type().is_file() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "update backup is not a regular file",
                    ))
                }
                Err(error) => return Err(error),
            }
            let replacement = transaction.join(ROLLBACK_DIRECTORY).join(path);
            if let Some(parent) = replacement.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, &replacement)?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            replace_file_atomically(&replacement, &destination)?;
        } else {
            remove_file_if_present(&destination)?;
        }
    }

    fs::remove_file(transaction.join(TRANSACTION_RECORD_FILE))?;
    let _ = remove_directory_if_present(transaction);
    Ok(())
}

fn validate_original_paths(record: &TransactionRecord) -> io::Result<()> {
    let touched: BTreeSet<String> = record
        .plan
        .changed
        .iter()
        .map(|entry| manifest_path_key(&entry.path))
        .chain(
            record
                .plan
                .removed
                .iter()
                .map(|path| manifest_path_key(path)),
        )
        .collect();
    let mut originals = BTreeSet::new();
    for path in &record.original_files {
        let key = manifest_path_key(path);
        if !is_safe_manifest_path(path) || !touched.contains(&key) || !originals.insert(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update transaction contains an invalid original path",
            ));
        }
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(file_operation_error("remove", path, error)),
    }
}

fn remove_directory_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(file_operation_error("remove directory", path, error)),
    }
}

/// Identifies the rejected filesystem operation while preserving its error category.
fn file_operation_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("could not {action} {}: {error}", path.display()),
    )
}

fn validate_plan_paths(plan: &Plan) -> io::Result<()> {
    let manifest = Manifest {
        version: String::new(),
        files: plan.changed.clone(),
    };
    validate_manifest(&manifest)?;
    let mut removed_paths = BTreeSet::new();
    for path in &plan.removed {
        if !is_safe_manifest_path(path) || !removed_paths.insert(manifest_path_key(path)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("update plan contains an invalid removal path: {path}"),
            ));
        }
    }
    Ok(())
}

fn is_safe_manifest_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && path.split('/').next().is_some_and(|component| {
            manifest_path_key(component) != manifest_path_key(TRANSACTION_DIRECTORY)
                && manifest_path_key(component) != manifest_path_key(installation::LOCK_FILE)
        })
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && is_unambiguous_component(component)
        })
}

/// Rejects names that Windows normalizes to a different managed file or directory.
fn is_unambiguous_component(component: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        !component.ends_with(['.', ' '])
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = component;
        true
    }
}

fn validate_managed_path(root: &Path, relative: &str) -> io::Result<()> {
    let Some(parent) = Path::new(relative).parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_plain_directory(&metadata) => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "managed path has a non-directory parent: {}",
                        current.display()
                    ),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn metadata_is_plain_directory(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_dir() {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// Atomically replaces a destination with a prepared file on the same volume.
pub fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
        .map_err(|error| file_operation_error("replace", destination, error))
}

/// The correctness oracle: does every file the manifest lists exist under
/// `install_dir` with the exact bytes (by hash)? A successful patch must make
/// this return `true` — i.e. the patched install is bit-identical to a clean
/// full install of the same version.
pub fn verify_install(install_dir: &Path, manifest: &Manifest) -> io::Result<bool> {
    validate_manifest(manifest)?;
    for entry in &manifest.files {
        if !verify_file(install_dir, entry)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Verifies one owned file without following symbolic links or loading its complete contents.
fn verify_file(directory: &Path, entry: &FileEntry) -> io::Result<bool> {
    validate_managed_path(directory, &entry.path)?;
    let path = directory.join(&entry.path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(metadata.len() == entry.size && sha256_file(&path)? == entry.sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory blob store keyed by hash — stands in for remote/local blobs.
    struct MemBlobs(HashMap<String, Vec<u8>>);
    impl BlobSource for MemBlobs {
        fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()> {
            let bytes = self
                .0
                .get(&entry.sha256)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, entry.sha256.clone()))?;
            fs::write(destination, bytes)
        }
    }

    fn entry(path: &str, data: &[u8]) -> FileEntry {
        FileEntry {
            path: path.into(),
            sha256: sha256_hex(data),
            size: data.len() as u64,
        }
    }

    fn manifest(version: &str, files: &[(&str, &[u8])]) -> Manifest {
        Manifest {
            version: version.into(),
            files: files.iter().map(|(p, d)| entry(p, d)).collect(),
        }
    }

    fn store(files: &[(&str, &[u8])]) -> MemBlobs {
        let mut map = HashMap::new();
        for (_, data) in files {
            map.insert(sha256_hex(data), data.to_vec());
        }
        MemBlobs(map)
    }

    #[test]
    fn parses_gen_manifest_json() {
        // Byte-for-byte the shape content/gen-manifest.py writes.
        let json =
            br#"{"version":"0.0.2","files":[{"path":"catalog.xml","sha256":"aa","size":3}]}"#;
        let m = Manifest::from_json(json).unwrap();
        assert_eq!(m.version, "0.0.2");
        assert_eq!(m.files[0].path, "catalog.xml");
        assert_eq!(m.files[0].size, 3);
    }

    #[test]
    fn sha256_file_with_multiple_buffers_matches_byte_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-asset");
        let bytes: Vec<u8> = (0..200_003).map(|index| (index % 251) as u8).collect();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(&bytes));
    }

    #[test]
    fn diff_verified_with_damaged_unchanged_asset_includes_it_for_repair() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("asset"), b"bad").unwrap();
        let target = manifest("1", &[("asset", b"new")]);
        assert!(diff(Some(&target), &target).is_empty());
        let plan = diff_verified(Some(&target), &target, directory.path()).unwrap();
        assert_eq!(plan.changed, target.files);
    }

    #[test]
    fn apply_with_installation_lock_in_inventory_cannot_replace_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let _owner = InstallationLock::try_acquire(directory.path()).unwrap();
        let target = manifest("1", &[(installation::LOCK_FILE, b"replacement")]);
        let result = apply(
            &diff(None, &target),
            directory.path(),
            &store(&[(installation::LOCK_FILE, b"replacement")]),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            InstallationLock::try_acquire(directory.path())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            fs::metadata(directory.path().join(installation::LOCK_FILE))
                .unwrap()
                .len(),
            0
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn validate_manifest_with_windows_normalized_path_alias_rejects_inventory() {
        for path in [
            ".rebellion2-install.lock.",
            ".rebellion2-install.lock ",
            ".rebellion2-update-transaction./transaction.json",
            "Content /catalog.xml",
            "asset.bin.",
        ] {
            assert_eq!(
                validate_manifest(&manifest("1", &[(path, b"new")]))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData,
                "{path}"
            );
        }
    }

    #[test]
    fn apply_with_file_backed_source_stages_without_fetching_asset_into_memory() {
        struct FileBlobs;
        impl BlobSource for FileBlobs {
            fn fetch_to(&self, _: &FileEntry, destination: &Path) -> io::Result<()> {
                fs::write(destination, b"streamed")
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let target = manifest("1", &[("asset", b"streamed")]);
        apply(&diff(None, &target), directory.path(), &FileBlobs).unwrap();
        assert!(verify_install(directory.path(), &target).unwrap());
    }

    #[test]
    fn fresh_install_pulls_everything() {
        let remote = manifest("1", &[("a", b"A"), ("b", b"B")]);
        let plan = diff(None, &remote);
        assert_eq!(plan.changed.len(), 2);
        assert!(plan.removed.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn replace_file_atomically_with_shared_reader_preserves_old_handle_and_publishes_new_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("version");
        let staged = directory.path().join("staged");
        fs::write(&destination, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let mut reader = fs::File::open(&destination).unwrap();
        replace_file_atomically(&staged, &destination).unwrap();
        let mut old_bytes = Vec::new();
        reader.read_to_end(&mut old_bytes).unwrap();
        assert_eq!(old_bytes, b"old");
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn no_change_is_empty_plan() {
        let m = manifest("1", &[("a", b"A"), ("b", b"B")]);
        assert!(diff(Some(&m), &m).is_empty());
    }

    #[test]
    fn diff_detects_changed_added_removed() {
        let v1 = manifest("1", &[("a", b"A"), ("b", b"B"), ("c", b"C")]);
        let v2 = manifest("2", &[("a", b"A"), ("b", b"B2"), ("d", b"D")]);
        let plan = diff(Some(&v1), &v2);
        let changed: Vec<&str> = plan.changed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(changed, vec!["b", "d"]); // a unchanged, b changed, d added
        assert_eq!(plan.removed, vec!["c".to_string()]); // c deleted
                                                         // only the changed bytes, not the whole set
        assert_eq!(plan.download_size(), (b"B2".len() + b"D".len()) as u64);
    }

    #[test]
    fn download_size_saturates_when_manifest_sizes_exceed_u64() {
        let plan = Plan {
            changed: vec![
                FileEntry {
                    path: "a".to_string(),
                    sha256: "0".repeat(64),
                    size: u64::MAX,
                },
                FileEntry {
                    path: "b".to_string(),
                    sha256: "1".repeat(64),
                    size: 1,
                },
            ],
            removed: Vec::new(),
        };

        assert_eq!(plan.download_size(), u64::MAX);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn diff_does_not_remove_a_windows_path_whose_case_changed() {
        let local = manifest("1", &[("rebellion2.exe", b"old")]);
        let remote = manifest("2", &[("Rebellion2.exe", b"new")]);

        let plan = diff(Some(&local), &remote);

        assert_eq!(plan.changed, vec![entry("Rebellion2.exe", b"new")]);
        assert!(plan.removed.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn apply_does_not_delete_a_changed_path_whose_case_changed() {
        let install = tempfile::tempdir().unwrap();
        fs::write(install.path().join("rebellion2.exe"), b"old").unwrap();
        let replacement = entry("Rebellion2.exe", b"new");
        let plan = Plan {
            changed: vec![replacement.clone()],
            removed: vec!["rebellion2.exe".to_string()],
        };

        apply(&plan, install.path(), &store(&[("Rebellion2.exe", b"new")])).unwrap();

        assert_eq!(
            fs::read(install.path().join("Rebellion2.exe")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn corrupted_blob_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let remote = manifest("1", &[("a", b"A")]);
        // store returns the WRONG bytes for a's hash
        let mut map = HashMap::new();
        map.insert(remote.files[0].sha256.clone(), b"TAMPERED".to_vec());
        let err = apply(&diff(None, &remote), dir.path(), &MemBlobs(map)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // nothing partial left behind
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn blob_with_incorrect_declared_size_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut remote = manifest("1", &[("a", b"A")]);
        remote.files[0].size = 2;

        let error = apply(
            &diff(None, &remote),
            directory.path(),
            &store(&[("a", b"A")]),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!directory.path().join("a").exists());
    }

    #[test]
    fn apply_with_later_fetch_failure_keeps_every_live_file_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"old-a").unwrap();
        fs::write(directory.path().join("b"), b"old-b").unwrap();
        let target = manifest("2", &[("a", b"new-a"), ("b", b"new-b")]);
        let available = store(&[("a", b"new-a")]);

        let result = apply(&diff(None, &target), directory.path(), &available);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read(directory.path().join("a")).unwrap(), b"old-a");
        assert_eq!(fs::read(directory.path().join("b")).unwrap(), b"old-b");
        assert!(recover_interrupted_apply(directory.path()).unwrap());
        assert!(!directory.path().join(TRANSACTION_DIRECTORY).exists());
    }

    #[test]
    fn recover_interrupted_apply_restores_every_original_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("changed"), b"old-changed").unwrap();
        fs::write(directory.path().join("removed"), b"old-removed").unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        let staged = transaction.join(STAGED_DIRECTORY);
        let backup = transaction.join(BACKUP_DIRECTORY);
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::write(staged.join("changed"), b"new-changed").unwrap();
        fs::write(staged.join("added"), b"new-added").unwrap();
        fs::write(backup.join("changed"), b"old-changed").unwrap();
        fs::write(backup.join("removed"), b"old-removed").unwrap();
        let record = TransactionRecord {
            plan: Plan {
                changed: vec![
                    entry("changed", b"new-changed"),
                    entry("added", b"new-added"),
                ],
                removed: vec!["removed".to_string()],
            },
            original_files: vec!["changed".to_string(), "removed".to_string()],
        };
        write_transaction_record(&transaction, &record).unwrap();
        commit_transaction(directory.path(), &transaction, &record.plan).unwrap();
        assert_eq!(
            fs::read(directory.path().join("changed")).unwrap(),
            b"new-changed"
        );
        assert!(directory.path().join("added").is_file());
        assert!(!directory.path().join("removed").exists());

        assert!(recover_interrupted_apply(directory.path()).unwrap());

        assert_eq!(
            fs::read(directory.path().join("changed")).unwrap(),
            b"old-changed"
        );
        assert!(!directory.path().join("added").exists());
        assert_eq!(
            fs::read(directory.path().join("removed")).unwrap(),
            b"old-removed"
        );
        assert!(!transaction.exists());
    }

    #[test]
    fn recover_interrupted_apply_with_file_at_transaction_path_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::write(&transaction, b"not a transaction directory").unwrap();

        let error = recover_interrupted_apply(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(transaction).unwrap(),
            b"not a transaction directory"
        );
    }

    #[test]
    fn recover_interrupted_apply_with_directory_at_record_path_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::create_dir_all(transaction.join(TRANSACTION_RECORD_FILE)).unwrap();

        let error = recover_interrupted_apply(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(transaction.join(TRANSACTION_RECORD_FILE).is_dir());
    }

    #[test]
    fn recover_interrupted_apply_without_record_removes_abandoned_staging() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::create_dir_all(transaction.join(STAGED_DIRECTORY)).unwrap();
        fs::write(
            transaction.join(STAGED_DIRECTORY).join("partial"),
            b"partial",
        )
        .unwrap();

        assert!(recover_interrupted_apply(directory.path()).unwrap());
        assert!(!transaction.exists());
    }

    #[test]
    fn recover_interrupted_apply_with_malformed_record_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let live = directory.path().join("live");
        fs::write(&live, b"unchanged").unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::create_dir_all(&transaction).unwrap();
        fs::write(transaction.join(TRANSACTION_RECORD_FILE), b"not-json").unwrap();

        let error = recover_interrupted_apply(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(live).unwrap(), b"unchanged");
        assert!(transaction.exists());
    }

    #[test]
    fn apply_empty_plan_recovers_abandoned_transaction_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::create_dir_all(transaction.join(STAGED_DIRECTORY)).unwrap();

        assert_eq!(
            apply(&Plan::default(), directory.path(), &store(&[])).unwrap(),
            0
        );
        assert!(!transaction.exists());
    }

    #[test]
    fn recover_interrupted_apply_rejects_original_path_not_touched_by_plan() {
        let directory = tempfile::tempdir().unwrap();
        let transaction = directory.path().join(TRANSACTION_DIRECTORY);
        fs::create_dir_all(&transaction).unwrap();
        let record = TransactionRecord {
            plan: Plan {
                changed: vec![entry("changed", b"new")],
                removed: Vec::new(),
            },
            original_files: vec!["unrelated".to_string()],
        };
        write_transaction_record(&transaction, &record).unwrap();

        let error = recover_interrupted_apply(directory.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(transaction.exists());
    }

    #[test]
    fn apply_rejects_duplicate_changed_paths_before_fetching() {
        let directory = tempfile::tempdir().unwrap();
        let plan = Plan {
            changed: vec![entry("same", b"first"), entry("same", b"second")],
            removed: Vec::new(),
        };

        let error = apply(&plan, directory.path(), &store(&[])).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!directory.path().join(TRANSACTION_DIRECTORY).exists());
    }

    #[test]
    fn apply_rejects_duplicate_removed_paths_before_mutating_install() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("same"), b"keep").unwrap();
        let plan = Plan {
            changed: Vec::new(),
            removed: vec!["same".to_string(), "same".to_string()],
        };

        let error = apply(&plan, directory.path(), &store(&[])).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(directory.path().join("same")).unwrap(), b"keep");
    }

    #[test]
    fn apply_changed_path_also_listed_for_removal_keeps_replacement() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("same"), b"old").unwrap();
        let plan = Plan {
            changed: vec![entry("same", b"new")],
            removed: vec!["same".to_string()],
        };

        assert_eq!(
            apply(&plan, directory.path(), &store(&[("same", b"new")])).unwrap(),
            1
        );
        assert_eq!(fs::read(directory.path().join("same")).unwrap(), b"new");
    }

    #[test]
    fn apply_with_directory_at_destination_does_not_delete_destination() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("target")).unwrap();
        let target = manifest("2", &[("target", b"replacement")]);

        let error = apply(
            &diff(None, &target),
            directory.path(),
            &store(&[("target", b"replacement")]),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(directory.path().join("target").is_dir());
    }

    #[test]
    fn manifest_rejects_paths_outside_its_install_root() {
        for path in [
            "../outside",
            "/absolute",
            "nested\\windows",
            "C:/absolute",
            "nested//file",
            "nested/./file",
            "nested/../outside",
            ".rebellion2-update-transaction/staged/file",
        ] {
            let manifest = manifest("1", &[(path, b"data")]);
            assert!(
                validate_manifest_paths(&manifest).is_err(),
                "accepted {path}"
            );
        }
    }

    #[test]
    fn manifest_rejects_sha256_values_that_are_not_lowercase_hex() {
        for sha256 in [
            "../../outside",
            "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD",
            "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
            "abcd",
        ] {
            let manifest = Manifest {
                version: "1".to_string(),
                files: vec![FileEntry {
                    path: "a".to_string(),
                    sha256: sha256.to_string(),
                    size: 1,
                }],
            };

            assert!(validate_manifest(&manifest).is_err(), "accepted {sha256}");
        }
    }

    #[test]
    fn verify_install_rejects_incorrect_declared_size() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"A").unwrap();
        let mut target = manifest("1", &[("a", b"A")]);
        target.files[0].size = 2;

        assert!(!verify_install(directory.path(), &target).unwrap());
    }

    #[test]
    fn verify_install_rejects_missing_and_hash_mismatched_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = manifest("1", &[("a", b"A")]);

        assert!(!verify_install(directory.path(), &target).unwrap());

        fs::write(directory.path().join("a"), b"B").unwrap();
        assert!(!verify_install(directory.path(), &target).unwrap());

        fs::write(directory.path().join("a"), b"A").unwrap();
        assert!(verify_install(directory.path(), &target).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn apply_with_symlink_at_destination_does_not_follow_or_replace_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, directory.path().join("target")).unwrap();
        let target = manifest("2", &[("target", b"replacement")]);

        let error = apply(
            &diff(None, &target),
            directory.path(),
            &store(&[("target", b"replacement")]),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(outside).unwrap(), b"outside");
        assert!(directory.path().join("target").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn apply_with_symlinked_parent_does_not_write_outside_install_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), directory.path().join("nested")).unwrap();
        let target = manifest("2", &[("nested/target", b"replacement")]);

        let error = apply(
            &diff(None, &target),
            directory.path(),
            &store(&[("nested/target", b"replacement")]),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!outside.path().join("target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn verify_install_rejects_symlinked_file_and_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target"), b"A").unwrap();

        symlink(outside.path().join("target"), directory.path().join("file")).unwrap();
        assert!(!verify_install(directory.path(), &manifest("1", &[("file", b"A")])).unwrap());

        symlink(outside.path(), directory.path().join("nested")).unwrap();
        assert!(
            verify_install(directory.path(), &manifest("1", &[("nested/target", b"A")])).is_err()
        );
    }

    #[test]
    fn apply_rejects_unsafe_removal_paths() {
        let directory = tempfile::tempdir().unwrap();
        let plan = Plan {
            changed: Vec::new(),
            removed: vec!["../outside".to_string()],
        };

        let error = apply(&plan, directory.path(), &store(&[])).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// THE invariant, in the real engine: patch a v1 install to v2 by fetching
    /// only changed blobs, then assert the result is bit-identical to a clean
    /// v2 install.
    #[test]
    fn patch_v1_to_v2_is_bit_identical_to_clean_v2() {
        let v1_files: &[(&str, &[u8])] = &[
            ("catalog.xml", b"<c v=1>"),
            ("Packs/Foo/a.xml", b"AAA"),
            ("Packs/Foo/b.xml", b"BBB"),
            ("Packs/Foo/c.xml", b"CCC"), // deleted in v2
        ];
        let v2_files: &[(&str, &[u8])] = &[
            ("catalog.xml", b"<c v=2>"),   // changed
            ("Packs/Foo/a.xml", b"AAA"),   // unchanged
            ("Packs/Foo/b.xml", b"BBB-X"), // changed
            ("Packs/Foo/d.xml", b"DDD"),   // added
        ];
        let v1 = manifest("0.0.1", v1_files);
        let v2 = manifest("0.0.2", v2_files);

        // Lay down a v1 install.
        let install = tempfile::tempdir().unwrap();
        for (path, data) in v1_files {
            let p = install.path().join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        assert!(verify_install(install.path(), &v1).unwrap());

        // Patch v1 -> v2 using only the blob store.
        let plan = diff(Some(&v1), &v2);
        assert_eq!(plan.changed.len(), 3); // catalog, b, d — NOT a
        assert_eq!(plan.removed, vec!["Packs/Foo/c.xml".to_string()]);
        let written = apply(&plan, install.path(), &store(v2_files)).unwrap();
        assert_eq!(written, 3);

        // The oracle: patched install == clean v2 install, and the deleted file is gone.
        assert!(verify_install(install.path(), &v2).unwrap());
        assert!(!install.path().join("Packs/Foo/c.xml").exists());

        // Idempotency: re-diff against v2 is now empty.
        assert!(diff(Some(&v2), &v2).is_empty());
    }
}
