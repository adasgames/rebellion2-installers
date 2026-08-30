//! Content update engine for the Rebellion II launcher.
//!
//! Pure logic, no Tauri and no network: the launcher shell injects a
//! [`BlobSource`] (HTTP against R2, a local dir, whatever) and this crate does
//! the manifest diff, hash-verified apply, and integrity check. Keeping it
//! separate makes the risky part — "did we patch the install correctly?" —
//! unit-testable without building the whole desktop app.
//!
//! The manifest format is exactly what `content/gen-manifest.py` emits:
//! `{"version": "...", "files": [{"path", "sha256", "size"}]}`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

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

    fn by_path(&self) -> BTreeMap<&str, &FileEntry> {
        self.files.iter().map(|f| (f.path.as_str(), f)).collect()
    }
}

/// The set of operations to turn a local install into the target version:
/// files to (re)write, and files to delete.
#[derive(Debug, Default, PartialEq, Eq)]
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
        self.changed.iter().map(|f| f.size).sum()
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
        match current.get(file.path.as_str()) {
            Some(existing) if existing.sha256 == file.sha256 => {}
            _ => changed.push(file.clone()),
        }
    }

    let mut removed: Vec<String> = current
        .keys()
        .filter(|path| !target.contains_key(**path))
        .map(|path| path.to_string())
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
/// HTTP-backed impl (R2); tests supply an in-memory or local-dir impl.
pub trait BlobSource {
    fn fetch(&self, sha256: &str) -> io::Result<Vec<u8>>;
}

/// Apply a plan under `install_dir`. Every fetched blob is hash-checked before
/// it's written, and each file lands via write-temp-then-rename so a partial
/// write can never surface as a live file. Deleted paths are removed last.
///
/// Returns the number of files written.
pub fn apply(plan: &Plan, install_dir: &Path, blobs: &dyn BlobSource) -> io::Result<usize> {
    for entry in &plan.changed {
        let bytes = blobs.fetch(&entry.sha256)?;
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
        let dst = install_dir.join(&entry.path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = PathBuf::from(format!("{}.part", dst.display()));
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &dst)?; // atomic replace of this file
    }

    for path in &plan.removed {
        let victim = install_dir.join(path);
        if victim.exists() {
            fs::remove_file(&victim)?;
        }
    }

    Ok(plan.changed.len())
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

    /// In-memory blob store keyed by hash — stands in for R2/local blobs.
    struct MemBlobs(HashMap<String, Vec<u8>>);
    impl BlobSource for MemBlobs {
        fn fetch(&self, sha256: &str) -> io::Result<Vec<u8>> {
            self.0
                .get(sha256)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, sha256.to_string()))
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
        let json = br#"{"version":"0.0.2","files":[{"path":"catalog.xml","sha256":"aa","size":3}]}"#;
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
