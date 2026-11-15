use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const RECORD_FILE: &str = ".modio.json";
const PAGE_SIZE: usize = 20;

/// Public game credentials supplied by development or release builds.
#[derive(Clone)]
pub struct Configuration {
    pub game_id: i64,
    api_key: String,
    base_url: String,
}

impl Configuration {
    /// Reads runtime overrides and falls back to public credentials embedded during compilation.
    pub fn load() -> io::Result<Self> {
        let game_id = setting("MODIO_GAME_ID", option_env!("MODIO_GAME_ID"))
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("Community mod browsing is unavailable in this build."))?;
        let api_key = setting("MODIO_API_KEY", option_env!("MODIO_API_KEY"))
            .ok_or_else(|| invalid("Community mod browsing is unavailable in this build."))?;
        let base_url = setting("MODIO_API_BASE_URL", option_env!("MODIO_API_BASE_URL"))
            .unwrap_or_else(|| format!("https://g-{game_id}.modapi.io/v1"));
        if !base_url.starts_with("https://") {
            return Err(invalid("The mod.io API address must use HTTPS."));
        }
        Ok(Self {
            game_id,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Reads a searchable page of publicly available mods.
    pub fn browse(&self, search: &str, offset: usize) -> io::Result<Page> {
        let request = self
            .request("mods")
            .query("_limit", &PAGE_SIZE.to_string())
            .query("_offset", &offset.to_string())
            .query("_sort", "-date_live");
        let request = if search.trim().is_empty() {
            request
        } else {
            request.query("name-lk", &format!("*{}*", search.trim()))
        };
        let response = request.call().map_err(request_error)?;
        serde_json::from_reader(response.into_reader()).map_err(io::Error::other)
    }

    /// Refreshes a mod before using its current published archive and temporary download URL.
    pub fn get_mod(&self, id: i64) -> io::Result<CommunityMod> {
        if id <= 0 {
            return Err(invalid("The mod identifier is invalid."));
        }
        let response = self
            .request(&format!("mods/{id}"))
            .call()
            .map_err(request_error)?;
        let entry: CommunityMod =
            serde_json::from_reader(response.into_reader()).map_err(io::Error::other)?;
        if entry.id != id || entry.game_id != self.game_id {
            return Err(invalid("This mod does not belong to this game."));
        }
        Ok(entry)
    }

    fn request(&self, path: &str) -> ureq::Request {
        ureq::get(&format!("{}/games/{}/{path}", self.base_url, self.game_id))
            .set("User-Agent", "Rebellion2")
            .set("Accept", "application/json")
            .query("api_key", &self.api_key)
    }
}

#[derive(Clone, Deserialize)]
pub struct Page {
    #[serde(rename = "data", default)]
    pub mods: Vec<CommunityMod>,
    pub result_offset: usize,
    pub result_limit: usize,
    pub result_total: usize,
}

#[derive(Clone, Deserialize)]
pub struct CommunityMod {
    pub id: i64,
    pub game_id: i64,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    pub modfile: Option<ModFile>,
}

#[derive(Clone, Deserialize)]
pub struct ModFile {
    pub id: i64,
    pub mod_id: i64,
    pub filesize: u64,
    #[serde(default)]
    pub version: Option<String>,
    pub filehash: FileHash,
    pub download: Download,
}

#[derive(Clone, Deserialize)]
pub struct FileHash {
    pub md5: String,
}

#[derive(Clone, Deserialize)]
pub struct Download {
    pub binary_url: String,
}

/// This schema is shared with the game's community mod installer.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Installation {
    pub schema_version: u32,
    pub game_id: i64,
    pub mod_id: i64,
    pub file_id: i64,
    pub content_id: String,
    pub content_version: String,
    pub name: String,
}

/// Reads a recognized managed installation without failing catalog browsing on damaged metadata.
pub fn installed(root: &Path, game_id: i64, mod_id: i64) -> Option<Installation> {
    if mod_id <= 0 {
        return None;
    }
    let record: Installation =
        serde_json::from_slice(&fs::read(mod_directory(root, mod_id).join(RECORD_FILE)).ok()?)
            .ok()?;
    (record.schema_version == 1 && record.game_id == game_id && record.mod_id == mod_id)
        .then_some(record)
}

/// Streams and verifies an archive before committing it into the shared Mods directory.
pub fn download_and_install(
    root: &Path,
    configuration: &Configuration,
    entry: &CommunityMod,
    progress: impl Fn(u64, u64),
) -> io::Result<()> {
    let file = entry
        .modfile
        .as_ref()
        .ok_or_else(|| invalid("This mod has no archive available to download."))?;
    if entry.game_id != configuration.game_id
        || entry.id <= 0
        || file.id <= 0
        || file.mod_id != entry.id
    {
        return Err(invalid("The downloaded mod does not belong to this game."));
    }
    let staging_parent = root.join(".modio-staging");
    fs::create_dir_all(&staging_parent)?;
    let download = tempfile::Builder::new()
        .prefix("download-")
        .tempdir_in(&staging_parent)?;
    let archive = download.path().join("archive.zip");
    download_archive(file, &archive, progress)?;
    install_archive(root, configuration.game_id, entry, &archive)
}

fn download_archive(
    file: &ModFile,
    destination: &Path,
    progress: impl Fn(u64, u64),
) -> io::Result<()> {
    if !file.download.binary_url.starts_with("https://")
        || file.filesize == 0
        || file.filehash.md5.is_empty()
    {
        return Err(invalid(
            "This mod has no valid archive available to download.",
        ));
    }
    let response = ureq::get(&file.download.binary_url)
        .set("User-Agent", "Rebellion2")
        .call()
        .map_err(request_error)?;
    let mut input = response.into_reader();
    let mut output = File::create_new(destination)?;
    let mut hash = md5::Context::new();
    let mut buffer = [0_u8; 81920];
    let mut received = 0_u64;
    let mut reported_percent = None;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| invalid("The archive size is invalid."))?;
        if received > file.filesize {
            return Err(invalid(
                "The downloaded archive is larger than its published size.",
            ));
        }
        hash.consume(&buffer[..count]);
        output.write_all(&buffer[..count])?;
        let percent = (u128::from(received) * 100) / u128::from(file.filesize);
        if reported_percent != Some(percent) {
            reported_percent = Some(percent);
            progress(received, file.filesize);
        }
    }
    let digest = format!("{:x}", hash.compute());
    if received != file.filesize || !digest.eq_ignore_ascii_case(&file.filehash.md5) {
        return Err(invalid(
            "The downloaded archive failed verification. Please try again.",
        ));
    }
    Ok(())
}

/// Extracts and validates a verified package, restoring its previous installation on commit failure.
pub fn install_archive(
    root: &Path,
    game_id: i64,
    entry: &CommunityMod,
    archive: &Path,
) -> io::Result<()> {
    let file = entry
        .modfile
        .as_ref()
        .ok_or_else(|| invalid("The mod has no published archive."))?;
    if game_id <= 0
        || entry.game_id != game_id
        || entry.id <= 0
        || file.id <= 0
        || file.mod_id != entry.id
    {
        return Err(invalid("The downloaded mod does not belong to this game."));
    }
    let staging_parent = root.join(".modio-staging");
    fs::create_dir_all(&staging_parent)?;
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&staging_parent)?;
    let backup = staging.path().join("previous");
    let result = (|| {
        let extracted = staging.path().join("package");
        fs::create_dir(&extracted)?;
        extract(archive, &extracted)?;
        let package = package_root(&extracted)?;
        let identity = validate_package(root, &package)?;
        let destination = mod_directory(root, entry.id);
        let is_pack = package.join("pack.xml").is_file();
        if let Some(previous) = installed(root, game_id, entry.id) {
            if previous.content_id != identity.id
                || !destination
                    .join(if is_pack { "pack.xml" } else { "mod.xml" })
                    .is_file()
            {
                return Err(invalid(
                    "An update cannot change the installed package's content ID or package type.",
                ));
            }
        }
        reject_duplicate(root, &destination, &identity.id, is_pack)?;
        let record = Installation {
            schema_version: 1,
            game_id,
            mod_id: entry.id,
            file_id: file.id,
            content_id: identity.id,
            content_version: identity.version,
            name: entry.name.clone(),
        };
        for file in fs::read_dir(&package)? {
            let file = file?;
            if file
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(RECORD_FILE)
            {
                fs::remove_file(file.path())?;
            }
        }
        fs::write(
            package.join(RECORD_FILE),
            serde_json::to_vec(&record).map_err(io::Error::other)?,
        )?;
        fs::create_dir_all(root.join("Mods"))?;
        if destination.exists() {
            if fs::symlink_metadata(&destination)?.file_type().is_symlink()
                || installed(root, game_id, entry.id).is_none()
            {
                return Err(invalid(
                    "The destination contains an unrecognized local mod and will not be replaced.",
                ));
            }
            fs::rename(&destination, &backup)?;
        }
        if let Err(error) = fs::rename(&package, &destination) {
            if backup.exists() && !destination.exists() {
                fs::rename(&backup, &destination)?;
            }
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() && backup.exists() {
        let preserved = staging.path().to_path_buf();
        let _ = staging.keep();
        return Err(io::Error::other(format!(
            "Installation failed; the previous files were preserved at {}.",
            preserved.display()
        )));
    }
    result
}

fn extract(archive: &Path, destination: &Path) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
    let mut files = HashSet::new();
    let mut path_cases = HashMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry.name().replace('\\', "/");
        let segments: Vec<_> = relative.trim_end_matches('/').split('/').collect();
        if segments.iter().any(|segment| {
            segment.trim().is_empty()
                || matches!(*segment, "." | "..")
                || segment.contains([':', '*', '?', '"', '<', '>', '|', '\0'])
                || segment.ends_with(['.', ' '])
        }) || relative.starts_with('/')
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(invalid(
                "The mod archive contains an unsafe file path or symbolic link.",
            ));
        }
        let path = segments
            .iter()
            .fold(destination.to_path_buf(), |path, segment| {
                path.join(segment)
            });
        let mut prefix = String::new();
        for segment in &segments {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if path_cases
                .get(&prefix.to_lowercase())
                .is_some_and(|previous| previous != &prefix)
            {
                return Err(invalid(
                    "The mod archive contains paths that differ only by letter case.",
                ));
            }
            path_cases.insert(prefix.to_lowercase(), prefix.clone());
        }
        if relative.ends_with('/') {
            fs::create_dir_all(path)?;
            continue;
        }
        if !files.insert(relative.to_lowercase()) {
            return Err(invalid("The mod archive contains duplicate files."));
        }
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| invalid("The archive path is invalid."))?,
        )?;
        let mut output = File::create_new(path)?;
        io::copy(&mut entry, &mut output)?;
    }
    Ok(())
}

fn has_definition(path: &Path) -> bool {
    path.join("pack.xml").is_file() || path.join("mod.xml").is_file()
}

fn package_root(extracted: &Path) -> io::Result<PathBuf> {
    if has_definition(extracted) {
        return Ok(extracted.to_path_buf());
    }
    let entries: Vec<_> = fs::read_dir(extracted)?.collect::<io::Result<_>>()?;
    let directories: Vec<_> = entries
        .iter()
        .filter(|entry| entry.path().is_dir() && entry.file_name() != "__MACOSX")
        .collect();
    if directories.len() == 1
        && has_definition(&directories[0].path())
        && entries
            .iter()
            .all(|entry| entry.path().is_dir() || entry.file_name() == ".DS_Store")
    {
        return Ok(directories[0].path());
    }
    Err(invalid(
        "The ZIP must contain one pack.xml or mod.xml package.",
    ))
}

#[derive(Deserialize)]
struct Definition {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
    #[serde(rename = "BasePackID", default)]
    base_id: String,
    #[serde(rename = "BasePackVersion", default)]
    base_version: String,
}

fn read_definition(path: &Path, expected_root: &str) -> io::Result<Definition> {
    let xml = fs::read_to_string(path)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut root_seen = false;
    let mut depth = 0_usize;
    loop {
        match reader.read_event().map_err(io::Error::other)? {
            quick_xml::events::Event::DocType(_) => {
                return Err(invalid(
                    "Content definitions cannot contain a document type.",
                ))
            }
            quick_xml::events::Event::Start(event) => {
                if !root_seen && event.name().as_ref() != expected_root.as_bytes() {
                    return Err(invalid(
                        "The package definition has an unexpected root element.",
                    ));
                }
                if root_seen && expected_root == "ContentModDefinition" {
                    validate_overlay_field(event.name().as_ref(), depth)?;
                }
                root_seen = true;
                depth += 1;
            }
            quick_xml::events::Event::Empty(event)
                if expected_root == "ContentModDefinition" && root_seen =>
            {
                validate_overlay_field(event.name().as_ref(), depth)?
            }
            quick_xml::events::Event::End(_) => depth = depth.saturating_sub(1),
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    let definition: Definition = quick_xml::de::from_str(&xml).map_err(io::Error::other)?;
    if definition.id.trim().is_empty() || definition.version.trim().is_empty() {
        return Err(invalid(
            "The content definition must declare its ID and version.",
        ));
    }
    Ok(definition)
}

fn validate_overlay_field(name: &[u8], depth: usize) -> io::Result<()> {
    if depth != 1
        || !matches!(
            name,
            b"ID" | b"Version" | b"DisplayName" | b"BasePackID" | b"BasePackVersion"
        )
    {
        return Err(invalid(
            "The mod definition contains an unsupported metadata field.",
        ));
    }
    Ok(())
}

fn validate_package(root: &Path, package: &Path) -> io::Result<Definition> {
    let pack = package.join("pack.xml").is_file();
    let overlay = package.join("mod.xml").is_file();
    if pack == overlay {
        return Err(invalid(
            "The package must contain either pack.xml or mod.xml.",
        ));
    }
    let definition = read_definition(
        &package.join(if pack { "pack.xml" } else { "mod.xml" }),
        if pack {
            "ContentPack"
        } else {
            "ContentModDefinition"
        },
    )?;
    if pack && definition.base_id == definition.id {
        return Err(invalid("A content pack cannot inherit from itself."));
    }
    if overlay
        && (definition.display_name.trim().is_empty()
            || definition.base_id.trim().is_empty()
            || definition.base_version.trim().is_empty()
            || !package.join("Content").is_dir())
    {
        return Err(invalid(
            "The mod must declare a base pack and version and contain a Content folder.",
        ));
    }
    if !definition.base_id.trim().is_empty()
        && !base_installed(
            root,
            &definition.base_id,
            if overlay {
                Some(&definition.base_version)
            } else {
                None
            },
        )?
    {
        return Err(invalid(&format!(
            "This mod requires content pack '{}'{}. Install that pack first.",
            definition.base_id,
            if overlay {
                format!(" version {}", definition.base_version)
            } else {
                String::new()
            }
        )));
    }
    if pack {
        validate_base_chain(root, &definition.id, &definition.base_id)?;
    }
    Ok(definition)
}

fn validate_base_chain(root: &Path, id: &str, base_id: &str) -> io::Result<()> {
    let mut visited = HashSet::from([id.to_string()]);
    let mut next = base_id.to_string();
    while !next.trim().is_empty() {
        if !visited.insert(next.clone()) {
            return Err(invalid(
                "The content pack has a circular base-pack dependency.",
            ));
        }
        let mut matches = Vec::new();
        for parent in [root.join("Content/Packs"), root.join("Mods")] {
            if !parent.is_dir() {
                continue;
            }
            for entry in fs::read_dir(parent)? {
                if let Ok(definition) =
                    read_definition(&entry?.path().join("pack.xml"), "ContentPack")
                {
                    if definition.id == next {
                        matches.push(definition);
                    }
                }
            }
        }
        if matches.len() != 1 {
            return Err(invalid(
                "A required base pack is missing or has multiple installed copies.",
            ));
        }
        next = matches.remove(0).base_id;
    }
    Ok(())
}

fn base_installed(root: &Path, id: &str, version: Option<&str>) -> io::Result<bool> {
    for parent in [root.join("Content/Packs"), root.join("Mods")] {
        if !parent.is_dir() {
            continue;
        }
        for entry in fs::read_dir(parent)? {
            let path = entry?.path().join("pack.xml");
            if !path.is_file() {
                continue;
            }
            if let Ok(definition) = read_definition(&path, "ContentPack") {
                if definition.id == id
                    && version.is_none_or(|version| definition.version == version)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn reject_duplicate(root: &Path, destination: &Path, id: &str, is_pack: bool) -> io::Result<()> {
    let mut parents = vec![root.join("Mods")];
    if is_pack {
        parents.push(root.join("Content/Packs"));
    }
    for parent in parents.into_iter().filter(|parent| parent.is_dir()) {
        for entry in fs::read_dir(parent)? {
            let path = entry?.path();
            if path == destination {
                continue;
            }
            let (file, kind) = if is_pack {
                ("pack.xml", "ContentPack")
            } else {
                ("mod.xml", "ContentModDefinition")
            };
            if let Ok(definition) = read_definition(&path.join(file), kind) {
                if definition.id == id {
                    return Err(invalid(&format!("A local copy of '{id}' is already installed. Remove that copy before installing this one.")));
                }
            }
        }
    }
    Ok(())
}

fn mod_directory(root: &Path, id: i64) -> PathBuf {
    root.join("Mods").join(format!("modio-{id}"))
}

fn setting(name: &str, baked: Option<&str>) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            baked
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
}

fn request_error(error: ureq::Error) -> io::Error {
    match error {
        ureq::Error::Status(status, _) => io::Error::other(format!(
            "mod.io could not complete the request (HTTP {status}). Please try again."
        )),
        ureq::Error::Transport(_) => {
            io::Error::other("Could not connect to mod.io. Check your connection and try again.")
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn installation() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("Content/Packs/Base");
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("pack.xml"),
            "<ContentPack><ID>base</ID><Version>1</Version></ContentPack>",
        )
        .unwrap();
        root
    }

    fn entry(file_id: i64) -> CommunityMod {
        CommunityMod {
            id: 12,
            game_id: 7,
            name: "Community".to_string(),
            summary: String::new(),
            modfile: Some(ModFile {
                id: file_id,
                mod_id: 12,
                filesize: 1,
                version: Some("1".to_string()),
                filehash: FileHash { md5: String::new() },
                download: Download {
                    binary_url: String::new(),
                },
            }),
        }
    }

    fn definition(version: &str, base_version: &str) -> String {
        format!("<ContentModDefinition><ID>community</ID><Version>{version}</Version><DisplayName>Community</DisplayName><BasePackID>base</BasePackID><BasePackVersion>{base_version}</BasePackVersion></ContentModDefinition>")
    }

    fn archive(root: &Path, entries: &[(&str, &str)]) -> PathBuf {
        let path = root.join("archive.zip");
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        for (name, contents) in entries {
            archive
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(contents.as_bytes()).unwrap();
        }
        archive.finish().unwrap();
        path
    }

    #[test]
    fn install_archive_overlay_package_writes_shared_record() {
        let root = installation();
        let xml = definition("1", "1");
        let archive = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/Pack/value.xml", "new")],
        );

        install_archive(root.path(), 7, &entry(44), &archive).unwrap();

        let record = installed(root.path(), 7, 12).unwrap();
        assert_eq!(record.content_id, "community");
        assert_eq!(record.file_id, 44);
        assert_eq!(
            fs::read_to_string(mod_directory(root.path(), 12).join("Content/Pack/value.xml"))
                .unwrap(),
            "new"
        );
    }

    #[test]
    fn install_archive_wrapped_pack_installs_definition_at_mod_root() {
        let root = installation();
        let archive = archive(
            root.path(),
            &[(
                "Wrapper/pack.xml",
                "<ContentPack><ID>new-pack</ID><Version>1</Version></ContentPack>",
            )],
        );

        install_archive(root.path(), 7, &entry(44), &archive).unwrap();

        assert!(mod_directory(root.path(), 12).join("pack.xml").is_file());
        assert_eq!(
            installed(root.path(), 7, 12).unwrap().content_id,
            "new-pack"
        );
    }

    #[test]
    fn install_archive_update_replaces_previous_files() {
        let root = installation();
        let xml = definition("1", "1");
        let initial = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/old.xml", "old")],
        );
        install_archive(root.path(), 7, &entry(44), &initial).unwrap();
        let xml = definition("2", "1");
        let update = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/new.xml", "new")],
        );

        install_archive(root.path(), 7, &entry(45), &update).unwrap();

        assert!(!mod_directory(root.path(), 12)
            .join("Content/old.xml")
            .exists());
        assert!(mod_directory(root.path(), 12)
            .join("Content/new.xml")
            .exists());
        assert_eq!(installed(root.path(), 7, 12).unwrap().file_id, 45);
    }

    #[test]
    fn install_archive_unsafe_paths_leave_installation_untouched() {
        for path in ["../escaped", "..\\escaped", "C:/escaped"] {
            let root = installation();
            let xml = definition("1", "1");
            let archive = archive(root.path(), &[("mod.xml", &xml), (path, "unsafe")]);

            assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());

            assert!(!mod_directory(root.path(), 12).exists());
            assert!(!root.path().join("escaped").exists());
        }
    }

    #[test]
    fn install_archive_conflicting_path_case_rejects_package() {
        let root = installation();
        let xml = definition("1", "1");
        let archive = archive(
            root.path(),
            &[
                ("mod.xml", &xml),
                ("Content/A.xml", "first"),
                ("content/B.xml", "second"),
            ],
        );

        assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());
    }

    #[test]
    fn install_archive_symbolic_link_rejects_package() {
        let root = installation();
        let path = root.path().join("link.zip");
        let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
        archive
            .add_symlink("link", "../outside", SimpleFileOptions::default())
            .unwrap();
        archive.finish().unwrap();

        assert!(install_archive(root.path(), 7, &entry(44), &path).is_err());
    }

    #[test]
    fn install_archive_stale_metadata_writes_current_record() {
        let root = installation();
        let xml = definition("1", "1");
        let archive = archive(
            root.path(),
            &[
                ("mod.xml", &xml),
                (RECORD_FILE, "{\"mod_id\":999}"),
                ("Content/value.xml", "new"),
            ],
        );

        install_archive(root.path(), 7, &entry(44), &archive).unwrap();
        assert_eq!(installed(root.path(), 7, 12).unwrap().mod_id, 12);
    }

    #[test]
    fn install_archive_unmanaged_directory_preserves_local_files() {
        let root = installation();
        let destination = mod_directory(root.path(), 12);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep.txt"), "local").unwrap();
        let xml = definition("1", "1");
        let archive = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "new")],
        );

        assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());

        assert_eq!(
            fs::read_to_string(destination.join("keep.txt")).unwrap(),
            "local"
        );
    }

    #[test]
    fn install_archive_local_duplicate_preserves_manual_mod() {
        let root = installation();
        let local = root.path().join("Mods/LocalCopy");
        fs::create_dir_all(&local).unwrap();
        let xml = definition("1", "1");
        fs::write(local.join("mod.xml"), &xml).unwrap();
        let archive = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "new")],
        );

        assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());

        assert!(local.join("mod.xml").exists());
        assert!(!mod_directory(root.path(), 12).exists());
    }

    #[test]
    fn install_archive_incompatible_update_preserves_old_version() {
        let root = installation();
        let xml = definition("1", "1");
        let initial = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "old")],
        );
        install_archive(root.path(), 7, &entry(44), &initial).unwrap();
        let xml = definition("2", "2");
        let update = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "new")],
        );

        assert!(install_archive(root.path(), 7, &entry(45), &update).is_err());

        assert_eq!(
            fs::read_to_string(mod_directory(root.path(), 12).join("Content/value.xml")).unwrap(),
            "old"
        );
        assert_eq!(installed(root.path(), 7, 12).unwrap().file_id, 44);
    }

    #[test]
    fn installed_game_record_reads_shared_archive_identity() {
        let root = installation();
        let destination = mod_directory(root.path(), 12);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join(RECORD_FILE), r#"{"schema_version":1,"game_id":7,"mod_id":12,"file_id":44,"content_id":"community","content_version":"1","name":"Community"}"#).unwrap();

        assert_eq!(installed(root.path(), 7, 12).unwrap().file_id, 44);
    }

    #[test]
    fn installed_damaged_record_does_not_claim_ownership() {
        let root = installation();
        let destination = mod_directory(root.path(), 12);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join(RECORD_FILE), "invalid").unwrap();

        assert!(installed(root.path(), 7, 12).is_none());
    }

    #[test]
    fn install_archive_builtin_pack_identity_preserves_base_pack() {
        let root = installation();
        let archive = archive(
            root.path(),
            &[(
                "pack.xml",
                "<ContentPack><ID>base</ID><Version>2</Version></ContentPack>",
            )],
        );

        assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());
        assert!(!mod_directory(root.path(), 12).exists());
    }

    #[test]
    fn install_archive_changed_identity_preserves_installed_version() {
        for changes_type in [false, true] {
            let root = installation();
            let xml = definition("1", "1");
            let initial = archive(
                root.path(),
                &[("mod.xml", &xml), ("Content/value.xml", "old")],
            );
            install_archive(root.path(), 7, &entry(44), &initial).unwrap();
            let update = if changes_type {
                archive(
                    root.path(),
                    &[(
                        "pack.xml",
                        "<ContentPack><ID>community</ID><Version>2</Version></ContentPack>",
                    )],
                )
            } else {
                let xml = definition("2", "1").replace("<ID>community</ID>", "<ID>renamed</ID>");
                archive(
                    root.path(),
                    &[("mod.xml", &xml), ("Content/value.xml", "new")],
                )
            };

            assert!(install_archive(root.path(), 7, &entry(45), &update).is_err());
            assert_eq!(
                installed(root.path(), 7, 12).unwrap().content_id,
                "community"
            );
            assert_eq!(
                fs::read_to_string(mod_directory(root.path(), 12).join("Content/value.xml"))
                    .unwrap(),
                "old"
            );
        }
    }

    #[test]
    fn install_archive_circular_pack_update_preserves_previous_pack() {
        let root = installation();
        let initial = archive(
            root.path(),
            &[(
                "pack.xml",
                "<ContentPack><ID>community</ID><Version>1</Version></ContentPack>",
            )],
        );
        install_archive(root.path(), 7, &entry(44), &initial).unwrap();
        let dependent = root.path().join("Content/Packs/Dependent");
        fs::create_dir_all(&dependent).unwrap();
        fs::write(dependent.join("pack.xml"), "<ContentPack><ID>dependent</ID><Version>1</Version><BasePackID>community</BasePackID></ContentPack>").unwrap();
        let update = archive(root.path(), &[("pack.xml", "<ContentPack><ID>community</ID><Version>2</Version><BasePackID>dependent</BasePackID></ContentPack>")]);

        assert!(install_archive(root.path(), 7, &entry(45), &update).is_err());
        assert_eq!(installed(root.path(), 7, 12).unwrap().content_version, "1");
    }

    #[test]
    fn install_archive_overlay_uses_separate_identity_namespace() {
        let root = installation();
        let xml = definition("1", "1").replace("<ID>community</ID>", "<ID>base</ID>");
        let archive = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "new")],
        );

        install_archive(root.path(), 7, &entry(44), &archive).unwrap();

        assert_eq!(installed(root.path(), 7, 12).unwrap().content_id, "base");
    }

    #[test]
    fn install_archive_unknown_overlay_metadata_rejects_package() {
        let root = installation();
        let xml = definition("1", "1").replace(
            "</ContentModDefinition>",
            "<Unsupported>value</Unsupported></ContentModDefinition>",
        );
        let archive = archive(
            root.path(),
            &[("mod.xml", &xml), ("Content/value.xml", "new")],
        );

        assert!(install_archive(root.path(), 7, &entry(44), &archive).is_err());
    }
}
