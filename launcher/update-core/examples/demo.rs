//! Watch the patcher work end to end, on the filesystem, with no network.
//!
//!   cargo run --example demo
//!
//! Simulates an installed v1, publishes a v2 that changes one small file, adds
//! one, and deletes one (leaving a big file untouched), then patches the install
//! and proves the result is bit-identical to a clean v2 — fetching only the
//! bytes that changed.

use rebellion2_update_core::{apply, diff, sha256_hex, verify_install, BlobSource, FileEntry, Manifest};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Blobs read from a local `blobs/<sha256>` directory (stands in for remote object storage).
struct DirBlobs(PathBuf);
impl BlobSource for DirBlobs {
    fn fetch(&self, sha256: &str) -> io::Result<Vec<u8>> {
        fs::read(self.0.join(sha256))
    }
}

fn manifest(version: &str, files: &[(&str, &[u8])]) -> Manifest {
    Manifest {
        version: version.into(),
        files: files
            .iter()
            .map(|(p, d)| FileEntry {
                path: (*p).into(),
                sha256: sha256_hex(d),
                size: d.len() as u64,
            })
            .collect(),
    }
}

fn lay_down(root: &Path, files: &[(&str, &[u8])]) {
    for (path, data) in files {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}

fn stage_blobs(dir: &Path, files: &[(&str, &[u8])]) {
    fs::create_dir_all(dir).unwrap();
    for (_, data) in files {
        fs::write(dir.join(sha256_hex(data)), data).unwrap();
    }
}

fn kb(bytes: u64) -> String {
    format!("{:.1} KB", bytes as f64 / 1024.0)
}

fn main() {
    let work = std::env::temp_dir().join("rebellion2-patcher-demo");
    let _ = fs::remove_dir_all(&work);
    let install = work.join("install");
    let blobs = work.join("blobs");

    // A big asset that DOESN'T change between versions (the whole point).
    let big = vec![b'#'; 5 * 1024 * 1024]; // 5 MiB

    let v1: &[(&str, &[u8])] = &[
        ("catalog.xml", b"<catalog version=\"1\"/>"),
        ("Application/Strategy/UI/Windows/welder_tab_active.png", b"WELDER-v1"),
        ("Packs/Classic/Shared/Data/game.xml", &big),
        ("Packs/Classic/Shared/Data/old-thing.xml", b"to be removed in v2"),
    ];
    let v2: &[(&str, &[u8])] = &[
        ("catalog.xml", b"<catalog version=\"2\"/>"),                          // changed
        ("Application/Strategy/UI/Windows/welder_tab_active.png", b"WELDER-v2-GREEN"), // changed
        ("Packs/Classic/Shared/Data/game.xml", &big),                          // UNCHANGED (5 MiB)
        ("Packs/Classic/Shared/Data/new-event.xml", b"a new event added in v2"), // added
        // old-thing.xml is gone -> removed
    ];

    println!("┌─ Rebellion II patcher — live demo ─────────────────────────────");
    println!("│ Installed:  v1  ({} files)", v1.len());
    lay_down(&install, v1);
    let m1 = manifest("0.0.1", v1);
    let m2 = manifest("0.0.2", v2);
    stage_blobs(&blobs, v2); // the publish side would have uploaded these

    let full: u64 = m2.files.iter().map(|f| f.size).sum();
    println!("│ Published:  v2  ({} files, {} total)", v2.len(), kb(full));
    println!("│");

    // 1) diff
    let plan = diff(Some(&m1), &m2);
    println!("│ 1. diff v1 → v2");
    for f in &plan.changed {
        let verb = if m1.files.iter().any(|e| e.path == f.path) { "changed" } else { "added  " };
        println!("│      {verb}  {}  ({})", f.path, kb(f.size));
    }
    for p in &plan.removed {
        println!("│      removed  {p}");
    }
    println!("│");
    println!(
        "│ 2. fetch: {} of {} bytes  ({} instead of the full {})",
        kb(plan.download_size()),
        kb(full),
        kb(plan.download_size()),
        kb(full),
    );
    let saved = 100.0 * (1.0 - plan.download_size() as f64 / full as f64);
    println!("│      → skipped the unchanged 5 MiB asset — {saved:.1}% less to download");
    println!("│");

    // 3) apply (hash-verified, atomic per file)
    let written = apply(&plan, &install, &DirBlobs(blobs)).unwrap();
    println!("│ 3. apply: wrote {written} files (hash-verified), removed {}", plan.removed.len());
    println!("│");

    // 4) the oracle
    let ok = verify_install(&install, &m2).unwrap();
    let ghost = install.join("Packs/Classic/Shared/Data/old-thing.xml").exists();
    println!("│ 4. verify: patched install == clean v2 install?  {}", if ok { "YES ✓" } else { "NO ✗" });
    println!("│           removed file actually gone?            {}", if !ghost { "YES ✓" } else { "NO ✗" });
    println!("└────────────────────────────────────────────────────────────────");

    let _ = fs::remove_dir_all(&work);
    assert!(ok && !ghost, "invariant failed");
}
