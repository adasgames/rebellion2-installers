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

/// Where blob bytes come from, keyed by their SHA-256. The launcher supplies an
/// HTTP-backed impl; tests supply an in-memory or local-dir impl.
pub trait BlobSource {
    fn fetch(&self, entry: &FileEntry) -> io::Result<Vec<u8>>;
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
    validate_plan_paths(plan)?;
    recover_interrupted_apply(install_dir)?;
    if plan.is_empty() {
        return Ok(0);
    }

    let transaction = install_dir.join(TRANSACTION_DIRECTORY);
    remove_directory_if_present(&transaction)?;
    let staged = transaction.join(STAGED_DIRECTORY);
    fs::create_dir_all(&staged)?;

    for entry in &plan.changed {
        let bytes = blobs.fetch(entry)?;
        if bytes.len() as u64 != entry.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "size mismatch for {}: expected {}, got {}",
                    entry.path,
                    entry.size,
                    bytes.len()
                ),
            ));
        }
        let actual = sha256_hex(&bytes);
        if actual != entry.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "hash mismatch for {}: expected {}, got {actual}",
                    entry.path, entry.sha256
                ),
            ));
        }
        let destination = staged.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(destination, bytes)?;
    }

    let effective_removed: Vec<&String> = plan
        .removed
        .iter()
        .filter(|path| {
            !plan
                .changed
                .iter()
                .any(|entry| manifest_path_key(&entry.path) == manifest_path_key(path))
        })
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

    if let Err(commit_error) = commit_transaction(install_dir, &transaction, &record.plan) {
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
        Ok(metadata) if metadata.is_dir() => {}
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
        let source = transaction.join(STAGED_DIRECTORY).join(&entry.path);
        let destination = install_dir.join(&entry.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        replace_file_atomically(&source, &destination)?;
    }

    for path in &plan.removed {
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
        let destination = install_dir.join(path);
        if original_paths.contains(&manifest_path_key(path)) {
            let source = transaction.join(BACKUP_DIRECTORY).join(path);
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
        })
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

/// Atomically replaces a destination with a prepared file on the same volume.
#[cfg(target_os = "windows")]
pub fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
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

/// Atomically replaces a destination with a prepared file on the same volume.
#[cfg(not(target_os = "windows"))]
pub fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

/// The correctness oracle: does every file the manifest lists exist under
/// `install_dir` with the exact bytes (by hash)? A successful patch must make
/// this return `true` — i.e. the patched install is bit-identical to a clean
/// full install of the same version.
pub fn verify_install(install_dir: &Path, manifest: &Manifest) -> io::Result<bool> {
    for entry in &manifest.files {
        let path = install_dir.join(&entry.path);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => return Ok(false),
        };
        if file.metadata()?.len() != entry.size {
            return Ok(false);
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        if sha256_hex(&buf) != entry.sha256 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// In-memory blob store keyed by hash — stands in for remote/local blobs.
    struct MemBlobs(HashMap<String, Vec<u8>>);
    impl BlobSource for MemBlobs {
        fn fetch(&self, entry: &FileEntry) -> io::Result<Vec<u8>> {
            self.0
                .get(&entry.sha256)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, entry.sha256.clone()))
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
    fn fresh_install_pulls_everything() {
        let remote = manifest("1", &[("a", b"A"), ("b", b"B")]);
        let plan = diff(None, &remote);
        assert_eq!(plan.changed.len(), 2);
        assert!(plan.removed.is_empty());
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
