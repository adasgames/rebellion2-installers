use crate::{apply, apply_verified, sha256_hex, BlobSource, FileEntry, Manifest, Plan};
use std::fs;
use std::io;
use std::path::Path;

/// Commits an update's files and its locally generated metadata in one transaction.
/// Metadata paths must be owned by the caller; downloaded manifests are validated
/// against their layer's ownership boundary before reaching this function.
pub fn apply_with_metadata(
    plan: &Plan,
    directory: &Path,
    source: &dyn BlobSource,
    metadata: &[(&str, &[u8])],
) -> io::Result<usize> {
    let mut changed = plan.changed.clone();
    changed.extend(metadata_entries(metadata));
    apply(
        &Plan {
            changed,
            removed: plan.removed.clone(),
        },
        directory,
        &MetadataSource {
            source: Some(source),
            metadata,
        },
    )?;
    Ok(plan.changed.len())
}

/// Commits assets and metadata together, retaining rollback data until every expected file verifies.
pub fn apply_verified_with_metadata(
    plan: &Plan,
    directory: &Path,
    source: &dyn BlobSource,
    metadata: &[(&str, &[u8])],
    expected: &Manifest,
) -> io::Result<usize> {
    let entries = metadata_entries(metadata);
    let mut changed = plan.changed.clone();
    changed.extend(entries.iter().cloned());
    let mut complete = expected.clone();
    complete.files.extend(entries);
    apply_verified(
        &Plan {
            changed,
            removed: plan.removed.clone(),
        },
        directory,
        &MetadataSource {
            source: Some(source),
            metadata,
        },
        &complete,
    )?;
    Ok(plan.changed.len())
}

/// Commits related local metadata files together when no asset changes are needed.
pub fn store_metadata(directory: &Path, metadata: &[(&str, &[u8])]) -> io::Result<()> {
    apply(
        &Plan {
            changed: metadata_entries(metadata),
            removed: Vec::new(),
        },
        directory,
        &MetadataSource {
            source: None,
            metadata,
        },
    )?;
    Ok(())
}

/// Builds the exact inventory of the locally generated documents being committed.
fn metadata_entries(metadata: &[(&str, &[u8])]) -> Vec<FileEntry> {
    metadata
        .iter()
        .map(|(path, bytes)| FileEntry {
            path: (*path).to_string(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
        })
        .collect()
}

struct MetadataSource<'a> {
    source: Option<&'a dyn BlobSource>,
    metadata: &'a [(&'a str, &'a [u8])],
}

impl BlobSource for MetadataSource<'_> {
    fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()> {
        match self.metadata.iter().find(|(path, _)| *path == entry.path) {
            Some((_, bytes)) => fs::write(destination, bytes),
            None => self
                .source
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "metadata blob is missing"))?
                .fetch_to(entry, destination),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source;
    impl BlobSource for Source {
        fn fetch_to(&self, _: &FileEntry, destination: &Path) -> io::Result<()> {
            fs::write(destination, b"new")
        }
    }

    #[test]
    fn apply_verified_with_metadata_when_unchanged_file_is_missing_restores_assets_and_version() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("asset"), b"old").unwrap();
        fs::write(directory.path().join("version"), b"1").unwrap();
        let changed = FileEntry {
            path: "asset".into(),
            sha256: sha256_hex(b"new"),
            size: 3,
        };
        let missing = FileEntry {
            path: "unchanged".into(),
            sha256: sha256_hex(b"keep"),
            size: 4,
        };
        let expected = Manifest {
            version: "2".into(),
            files: vec![changed.clone(), missing],
        };
        let plan = Plan {
            changed: vec![changed],
            removed: Vec::new(),
        };
        assert!(apply_verified_with_metadata(
            &plan,
            directory.path(),
            &Source,
            &[("version", b"2")],
            &expected
        )
        .is_err());
        assert_eq!(fs::read(directory.path().join("asset")).unwrap(), b"old");
        assert_eq!(fs::read(directory.path().join("version")).unwrap(), b"1");
        assert!(!directory
            .path()
            .join(".rebellion2-update-transaction")
            .exists());
    }

    #[test]
    fn apply_with_metadata_when_metadata_destination_is_invalid_preserves_all_live_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("asset"), b"old").unwrap();
        fs::create_dir(directory.path().join("version")).unwrap();
        let plan = Plan {
            changed: vec![FileEntry {
                path: "asset".into(),
                size: 3,
                sha256: sha256_hex(b"new"),
            }],
            removed: Vec::new(),
        };
        let result = apply_with_metadata(&plan, directory.path(), &Source, &[("version", b"2")]);
        assert!(result.is_err());
        assert_eq!(fs::read(directory.path().join("asset")).unwrap(), b"old");
    }

    #[test]
    fn apply_with_metadata_with_success_commits_both_files_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let plan = Plan {
            changed: vec![FileEntry {
                path: "asset".into(),
                size: 3,
                sha256: sha256_hex(b"new"),
            }],
            removed: Vec::new(),
        };
        assert_eq!(
            apply_with_metadata(&plan, directory.path(), &Source, &[("version", b"2")]).unwrap(),
            1
        );
        assert_eq!(fs::read(directory.path().join("asset")).unwrap(), b"new");
        assert_eq!(fs::read(directory.path().join("version")).unwrap(), b"2");
    }
}
