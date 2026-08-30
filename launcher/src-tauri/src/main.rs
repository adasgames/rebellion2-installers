// Rebellion II launcher (Tauri)
//
// Flow:
//   1. Ownership — reuse a cached session token if still valid; otherwise open the
//      content gate (Steam/GOG/GitHub), which mints a signed token we cache.
//   2. Scan — read the public channel pointer (latest.json) and compare to what's
//      installed: nothing installed / behind / current.
//   3. Act — first install or an incremental patch require the user to confirm;
//      "already current" just says so and offers Play.
//
// Content (manifest + blobs) is fetched with the token in an Authorization header,
// so it is never freely downloadable — only the version pointer is public.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use rebellion2_update_core::{apply, diff, BlobSource, Manifest};
use serde::Deserialize;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const GATE: &str = "https://rebellion2-content-gate.pages.dev/";

/// The content version this launcher was built for (REB2_CONTENT_VERSION).
const CONTENT_VERSION: Option<&str> = option_env!("REB2_CONTENT_VERSION");
/// Public base URL of the content channel (REB2_CONTENT_BASE_URL): holds the public
/// dist/latest.json and the token-gated manifest + blobs.
const CONTENT_BASE: Option<&str> = option_env!("REB2_CONTENT_BASE_URL");

const CONTENT_VERSION_FILE: &str = ".content-version";
const CONTENT_MANIFEST_FILE: &str = ".manifest.json";
/// Cached ownership session token, stored next to the launcher.
const SESSION_FILE: &str = ".session";

#[cfg(target_os = "windows")]
const GAME_EXE: &str = "Rebellion2.exe";
#[cfg(target_os = "linux")]
const GAME_EXE: &str = "Rebellion2";
#[cfg(target_os = "macos")]
const GAME_EXE: &str = "Rebellion2.app";

const RELEASES_URL: &str = "https://github.com/adasgames/rebellion2-installers/releases/latest";

/// Internal URL scheme the in-window buttons navigate to; intercepted in on_nav so
/// the HTML can drive Rust without extra IPC wiring.
const ACT: &str = "https://launcher.invalid/act";

/// The current session token + the pending action, set once we know what to do and
/// read when the user clicks a button in the webview.
static SESSION: Mutex<Option<String>> = Mutex::new(None);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// A newer launcher release, if one is published; the user confirms before it runs.
static PENDING_LAUNCHER: Mutex<Option<LauncherUpdate>> = Mutex::new(None);
/// Set when the user picks "Not now" so we stop re-offering the launcher update.
static LAUNCHER_UPDATE_DISMISSED: AtomicBool = AtomicBool::new(false);

/// ed25519 public key (hex) that must have signed a launcher-update installer.
const LAUNCHER_UPDATE_PUBKEY: &str =
    "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d";

/// The launcher-update pointer at `dist/launcher.json`.
#[derive(Debug, Clone, Deserialize)]
struct LauncherUpdate {
    version: String,
    /// URL of the new installer to download and run.
    url: String,
    /// Hex ed25519 signature over the installer bytes.
    signature: String,
}

#[derive(Clone)]
enum Pending {
    /// First install of the baked version via the gate's presigned zip.
    FirstInstall { url: String },
    /// Incremental patch to `latest` from `base`.
    Update { base: String, latest: Latest },
}

#[derive(Debug)]
struct ContentUnavailable;
impl std::fmt::Display for ContentUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "content for this launcher version is no longer available")
    }
}
impl std::error::Error for ContentUnavailable {}

/// The public channel pointer at `dist/latest.json`.
#[derive(Debug, Clone, Deserialize)]
struct Latest {
    version: String,
    manifest: String,
    #[serde(default = "default_blobs")]
    blobs: String,
}
fn default_blobs() -> String {
    "blobs/".to_string()
}

fn main() {
    // Seed the session token from cache if we have a non-expired one.
    if let Some(token) = read_cached_token() {
        *SESSION.lock().unwrap() = Some(token);
    }

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();

            // Ownership gates the DOWNLOAD, never the play. If the game is already
            // installed we go straight to the local screen and let the user launch —
            // sign-in is only needed for a first install (or an explicit --repair).
            let installed = install_dir()
                .map(|d| d.join("Content").join("catalog.xml").is_file())
                .unwrap_or(false);
            let repair = std::env::args().any(|arg| arg == "--repair");
            let need_signin = repair || (!installed && SESSION.lock().unwrap().is_none());

            if need_signin {
                // Open the gate; on_nav captures the token from /done, then scans.
                WebviewWindowBuilder::new(
                    app,
                    "main",
                    WebviewUrl::External(gate_landing().parse().unwrap()),
                )
                .title("Rebellion 2 Launcher")
                .inner_size(520.0, 700.0)
                .resizable(true)
                .on_navigation(move |url| on_nav(&handle, url))
                .build()?;
            } else {
                // Installed (or already signed in) — go straight to the scan. A scan
                // failure degrades to "launch what's installed", so play never blocks.
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("Rebellion 2 Launcher")
                    .inner_size(520.0, 700.0)
                    .resizable(true)
                    .on_navigation({
                        let h = handle.clone();
                        move |url| on_nav(&h, url)
                    })
                    .build()?;
                thread::spawn(move || scan_and_prompt(&handle));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Rebellion II launcher");
}

// -- navigation interception (gate result + in-window buttons) ----------------

fn on_nav(handle: &tauri::AppHandle, url: &tauri::Url) -> bool {
    let target = url.as_str();

    // GOG bounces the ?code= via its own page — capture and hand to the gate.
    if target.starts_with("https://embed.gog.com/on_login_success") {
        if let Some(code) = query(url, "code") {
            if let Some(window) = handle.get_webview_window("main") {
                let mut callback = format!("{GATE}callback/gog?code={}", urlencoding::encode(&code));
                if let Some(v) = CONTENT_VERSION.filter(|v| !v.is_empty()) {
                    callback.push_str(&format!("&v={}", urlencoding::encode(v)));
                }
                if let Ok(parsed) = callback.parse() {
                    let _ = window.navigate(parsed);
                }
            }
            return false;
        }
    }

    // Gate result: /done?ok=1&url=<presigned>&token=<session>
    if target.starts_with(&format!("{GATE}done")) {
        let ok = url
            .query_pairs()
            .any(|(k, v)| k == "ok" && v == "1");
        let presigned = query(url, "url");
        let token = query(url, "token");
        on_gate_result(handle, ok, presigned, token);
        return true;
    }

    // In-window button: intercept, don't navigate.
    if target.starts_with(ACT) {
        if let Some(choice) = query(url, "choice") {
            on_choice(handle, &choice);
        }
        return false;
    }

    true
}

fn query(url: &tauri::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// After the gate verifies ownership: cache the token (model A) and scan. Falls
/// back to legacy behavior (presigned zip, no token) so it still works against a
/// gate that has not yet been upgraded to mint tokens.
fn on_gate_result(
    handle: &tauri::AppHandle,
    ok: bool,
    presigned: Option<String>,
    token: Option<String>,
) {
    if !ok {
        log_line("[launcher] DENIED — this account does not own the title.");
        show_message(handle, "Not verified", "This account does not own the game.", None);
        return;
    }
    log_line("[launcher] VERIFIED via the content gate.");
    if let Some(token) = token {
        store_token(&token);
        *SESSION.lock().unwrap() = Some(token);
    }
    // Remember the presigned zip for a possible first install.
    if let Some(url) = presigned {
        *PENDING.lock().unwrap() = Some(Pending::FirstInstall { url });
    }
    // Repaint the window to our local status page, then scan.
    show_status_page(handle);
    let handle = handle.clone();
    thread::spawn(move || scan_and_prompt(&handle));
}

/// Handles a button click from any of the launcher screens.
fn on_choice(handle: &tauri::AppHandle, choice: &str) {
    match choice {
        "play" => {
            log_line("[launcher] Play clicked.");
            match launch_game() {
                Ok(true) => handle.exit(0),
                Ok(false) => show_message(
                    handle,
                    "Not installed",
                    "The game files aren't here yet — reinstall from the latest build.",
                    None,
                ),
                Err(err) => {
                    log_line(&format!("[launcher] failed to start the game: {err}"));
                    show_message(handle, "Couldn't start", "The game failed to start — see launcher.log.", None);
                }
            }
        }
        "quit" => handle.exit(0),
        "launcher-update" => {
            let upd = PENDING_LAUNCHER.lock().unwrap().clone();
            if let Some(upd) = upd {
                let handle = handle.clone();
                thread::spawn(move || run_launcher_update(&handle, &upd));
            }
        }
        "skip-launcher-update" => {
            LAUNCHER_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            let handle = handle.clone();
            thread::spawn(move || scan_and_prompt(&handle));
        }
        "install" => {
            let pending = PENDING.lock().unwrap().clone();
            if let Some(Pending::FirstInstall { url }) = pending {
                start_install(handle.clone(), url);
            }
        }
        "update" => {
            let pending = PENDING.lock().unwrap().clone();
            if let Some(Pending::Update { base, latest }) = pending {
                show_progress_ui(handle);
                let handle = handle.clone();
                thread::spawn(move || run_update(&handle, &base, &latest));
            }
        }
        _ => {}
    }
}

// -- launcher self-update ----------------------------------------------------

/// A newer launcher release if one is published, else None. Never fails hard:
/// any error means "no launcher update" so play/patch continues normally. Stays
/// dormant until dist/launcher.json exists on the channel.
fn check_launcher_update() -> Option<LauncherUpdate> {
    if LAUNCHER_UPDATE_DISMISSED.load(Ordering::Relaxed) {
        return None;
    }
    let base = content_base()?;
    let current = CONTENT_VERSION.filter(|v| !v.is_empty())?;
    let update: LauncherUpdate = fetch_json(&format!("{base}dist/launcher.json"), None).ok()?;
    version_gt(&update.version, current).then_some(update)
}

/// True if dotted version `a` is newer than `b` (numeric per component, missing = 0).
fn version_gt(a: &str, b: &str) -> bool {
    let parts = |s: &str| {
        s.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn show_launcher_update(handle: &tauri::AppHandle) {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=launcher-update\">Update launcher</a>\
         <a class=\"b secondary\" href=\"{a}?choice=skip-launcher-update\">Not now</a>",
        a = ACT,
    );
    write_screen(
        handle,
        &render(
            "Launcher update",
            false,
            "A new launcher is available.<br>Update it, or continue with the current one.",
            &buttons,
        ),
    );
}

/// Downloads the signed installer, verifies its signature, and runs it. The
/// installer closes + replaces this launcher, then relaunches it.
fn run_launcher_update(handle: &tauri::AppHandle, update: &LauncherUpdate) {
    show_progress_ui(handle);
    update_progress(handle, 10, "Downloading launcher update…");
    match download_and_verify_installer(update) {
        Ok(installer) => {
            update_progress(handle, 100, "Starting installer…");
            let spawned = {
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    const DETACHED_BREAKAWAY: u32 = 0x0000_0008 | 0x0100_0000;
                    std::process::Command::new(&installer)
                        .creation_flags(DETACHED_BREAKAWAY)
                        .spawn()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    std::process::Command::new(&installer).spawn()
                }
            };
            match spawned {
                Ok(_) => handle.exit(0),
                Err(err) => {
                    log_line(&format!("[launcher] couldn't start the installer: {err}"));
                    show_message(handle, "Update failed", "Couldn't start the installer — see launcher.log.", None);
                }
            }
        }
        Err(err) => {
            log_line(&format!("[launcher] launcher update failed: {err}"));
            show_message(
                handle,
                "Update failed",
                "The launcher update couldn't be verified — see launcher.log.",
                Some(("Continue", "skip-launcher-update")),
            );
        }
    }
}

fn download_and_verify_installer(
    update: &LauncherUpdate,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let mut bytes = Vec::new();
    ureq::get(&update.url).call()?.into_reader().read_to_end(&mut bytes)?;

    let key: [u8; 32] = hex::decode(LAUNCHER_UPDATE_PUBKEY)?
        .as_slice()
        .try_into()
        .map_err(|_| "bad pubkey length")?;
    let verifying = VerifyingKey::from_bytes(&key)?;
    let sig = Signature::from_slice(&hex::decode(&update.signature)?)?;
    verifying
        .verify(&bytes, &sig)
        .map_err(|_| "installer signature does not match")?;

    let dst = std::env::temp_dir().join("Rebellion2-Update-Setup.exe");
    fs::write(&dst, &bytes)?;
    Ok(dst)
}

// -- scan --------------------------------------------------------------------

/// Reads the channel pointer and decides what to show. Any network failure
/// degrades to "launch what's installed" rather than forcing a re-download.
fn scan_and_prompt(handle: &tauri::AppHandle) {
    update_status(handle, "Checking for updates…");

    // A launcher/binary update supersedes content — offer it first. Dormant unless
    // dist/launcher.json is published; any failure falls through to the content flow.
    if let Some(upd) = check_launcher_update() {
        log_line(&format!("[launcher] launcher update available: {}", upd.version));
        *PENDING_LAUNCHER.lock().unwrap() = Some(upd);
        show_launcher_update(handle);
        return;
    }
    let content_dir = match install_dir() {
        Ok(dir) => dir.join("Content"),
        Err(_) => return,
    };
    let installed_present = content_dir.join("catalog.xml").is_file();
    let installed_version = read_installed_version(&content_dir);

    let base = match content_base() {
        Some(base) => base,
        None => {
            // No live channel configured — fall back to gate/zip behavior.
            if installed_present {
                show_up_to_date(handle, installed_version.as_deref().unwrap_or("installed"));
            } else if PENDING.lock().unwrap().is_some() {
                show_ready_to_install(handle, None);
            }
            return;
        }
    };

    match fetch_latest(&base) {
        Ok(latest) => {
            if !installed_present {
                // First install: reuse the gate's presigned zip captured earlier.
                show_ready_to_install(handle, Some(&latest.version));
            } else if installed_version.as_deref() == Some(latest.version.as_str()) {
                log_line(&format!("[launcher] up to date ({}).", latest.version));
                show_up_to_date(handle, &latest.version);
            } else {
                log_line(&format!(
                    "[launcher] update available: {} -> {}",
                    installed_version.as_deref().unwrap_or("unknown"),
                    latest.version
                ));
                prompt_update(handle, &base, &latest, &content_dir);
            }
        }
        Err(err) => {
            log_line(&format!("[launcher] update check failed ({err}); offering to play installed."));
            if installed_present {
                show_up_to_date(handle, installed_version.as_deref().unwrap_or("installed"));
            }
        }
    }
}

/// Computes the update size, then shows the confirm prompt.
fn prompt_update(handle: &tauri::AppHandle, base: &str, latest: &Latest, content_dir: &Path) {
    let token = SESSION.lock().unwrap().clone();
    let remote: Option<Manifest> =
        fetch_json(&format!("{base}{}", latest.manifest), token.as_deref()).ok();
    let (files, bytes) = match &remote {
        Some(remote) => {
            let local = read_local_manifest(content_dir).or_else(|| {
                read_installed_version(content_dir).and_then(|v| {
                    fetch_json::<Manifest>(&format!("{base}dist/manifest-{v}.json"), token.as_deref())
                        .ok()
                })
            });
            let plan = diff(local.as_ref(), &remote);
            (plan.changed.len(), plan.download_size())
        }
        None => (0, 0),
    };

    *PENDING.lock().unwrap() = Some(Pending::Update {
        base: base.to_string(),
        latest: latest.clone(),
    });

    let detail = if bytes > 0 {
        format!("An update is available ({}).", human_bytes(bytes))
    } else {
        "An update is available.".to_string()
    };
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=update\">Update</a>\
         <a class=\"b secondary\" href=\"{a}?choice=play\">Launch Game</a>",
        a = ACT,
    );
    write_screen(handle, &render("Update available", false, &detail, &buttons));
}

// -- update / install work ---------------------------------------------------

fn run_update(handle: &tauri::AppHandle, base: &str, latest: &Latest) {
    match do_update(handle, base, latest) {
        Ok(fetched) => {
            log_line(&format!("[launcher] update to {} complete ({fetched} files).", latest.version));
            update_progress(handle, 100, "Update complete.");
            // Let the filled bar sit a beat, then land on the up-to-date screen.
            thread::sleep(Duration::from_millis(900));
            show_up_to_date(handle, &latest.version);
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
            // A missing manifest/blob during an update means the channel is
            // unreachable or misconfigured — NOT that this version is retired.
            log_line("[launcher] update content unavailable (404 from channel).");
            update_progress(handle, 0, "Update failed — content unavailable. Please try again later.");
        }
        Err(err) => {
            log_line(&format!("[launcher] update failed: {err}"));
            update_progress(handle, 0, "Update failed — see launcher.log");
        }
    }
}

fn do_update(
    handle: &tauri::AppHandle,
    base: &str,
    latest: &Latest,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content_dir = install_dir()?.join("Content");
    let token = SESSION.lock().unwrap().clone();
    update_progress(handle, 0, "Checking what changed…");

    let remote: Manifest = fetch_json(&format!("{base}{}", latest.manifest), token.as_deref())?;
    let local = read_local_manifest(&content_dir).or_else(|| {
        read_installed_version(&content_dir).and_then(|v| {
            fetch_json::<Manifest>(&format!("{base}dist/manifest-{v}.json"), token.as_deref()).ok()
        })
    });

    let plan = diff(local.as_ref(), &remote);
    if plan.is_empty() {
        store_manifest_and_version(&content_dir, &remote, &latest.version)?;
        return Ok(0);
    }
    let changed = plan.changed.len();
    let bytes = plan.download_size();
    update_progress(handle, 5, &format!("Downloading {changed} files, {}", human_bytes(bytes)));

    let blobs = HttpBlobs {
        base: format!("{base}{}", latest.blobs),
        token,
        handle: handle.clone(),
        total: bytes,
        done: AtomicU64::new(0),
    };
    apply(&plan, &content_dir, &blobs)?;
    store_manifest_and_version(&content_dir, &remote, &latest.version)?;
    Ok(changed)
}

/// Token-authenticated blob source with cumulative download progress.
struct HttpBlobs {
    base: String,
    token: Option<String>,
    handle: tauri::AppHandle,
    total: u64,
    done: AtomicU64,
}
impl BlobSource for HttpBlobs {
    fn fetch(&self, sha256: &str) -> io::Result<Vec<u8>> {
        let mut request = ureq::get(&format!("{}{sha256}", self.base));
        if let Some(token) = &self.token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(401 | 403, _)) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unauthorized"))
            }
            Err(other) => return Err(io::Error::new(io::ErrorKind::Other, other.to_string())),
        };
        // Stream in chunks so the progress bar fills smoothly through a large
        // file, updating only when the whole-percent changes (≈90 evals total).
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        let mut buf = [0u8; 65536];
        let mut last_percent = u64::MAX;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            let done = self.done.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
            let percent = if self.total > 0 {
                (5 + done.saturating_mul(90) / self.total).min(95)
            } else {
                50
            };
            if percent != last_percent {
                last_percent = percent;
                update_progress(
                    &self.handle,
                    percent,
                    &format!("Downloading update… {} / {}", human_bytes(done), human_bytes(self.total)),
                );
            }
        }
        Ok(bytes)
    }
}

/// Downloads and extracts the full Content zip (first install), then stores the
/// version's manifest so future runs patch instead of re-pulling.
fn start_install(handle: tauri::AppHandle, url: String) {
    show_progress_ui(&handle);
    thread::spawn(move || match install_content(&handle, &url) {
        Ok(content_dir) => {
            log_line(&format!("[launcher] Content installed to {}", content_dir.display()));
            if let (Some(base), Some(version)) =
                (content_base(), CONTENT_VERSION.filter(|v| !v.is_empty()))
            {
                let token = SESSION.lock().unwrap().clone();
                if let Ok(manifest) = fetch_json::<Manifest>(
                    &format!("{base}dist/manifest-{version}.json"),
                    token.as_deref(),
                ) {
                    let _ = store_manifest_and_version(&content_dir, &manifest, version);
                }
            }
            update_progress(&handle, 100, "Starting game…");
            match launch_game() {
                Ok(true) => handle.exit(0),
                Ok(false) => show_message(&handle, "Installed", "The game is ready to play.", Some(("Launch Game", "play"))),
                Err(err) => log_line(&format!("[launcher] failed to start the game: {err}")),
            }
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => show_update_required_ui(&handle),
        Err(err) => {
            log_line(&format!("[launcher] failed to install Content: {err}"));
            update_progress(&handle, 0, "Download failed — see launcher.log");
        }
    });
}

// -- session token + channel helpers -----------------------------------------

/// Reads the cached session token if present and not obviously expired. The token
/// is `<base64url payload>.<sig>`; the payload carries an `exp` unix seconds. We
/// only pre-screen expiry here — the server re-validates the signature.
fn read_cached_token() -> Option<String> {
    let path = install_dir().ok()?.join(SESSION_FILE);
    let token = fs::read_to_string(path).ok()?.trim().to_string();
    if token.is_empty() || token_expired(&token) {
        return None;
    }
    Some(token)
}

fn store_token(token: &str) {
    if let Ok(base) = install_dir() {
        let _ = fs::write(base.join(SESSION_FILE), token);
    }
}

/// Best-effort expiry pre-check: decode the token payload and read `exp`. A token
/// we can't parse is treated as expired so we re-verify.
fn token_expired(token: &str) -> bool {
    let payload = token.split('.').next().unwrap_or("");
    let Some(exp) = decode_token_exp(payload) else {
        return true;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now >= exp
}

fn decode_token_exp(payload_b64url: &str) -> Option<u64> {
    let bytes = base64url_decode(payload_b64url)?;
    let text = String::from_utf8(bytes).ok()?;
    #[derive(Deserialize)]
    struct Payload {
        exp: u64,
    }
    serde_json::from_str::<Payload>(&text).ok().map(|p| p.exp)
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lut = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        lut[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input.trim_end_matches('=').as_bytes() {
        let v = lut[c as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn content_base() -> Option<String> {
    CONTENT_BASE
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| if v.ends_with('/') { v.to_string() } else { format!("{v}/") })
}

fn fetch_latest(base: &str) -> Result<Latest, Box<dyn std::error::Error>> {
    fetch_json(&format!("{base}dist/latest.json"), None)
}

fn fetch_json<T: serde::de::DeserializeOwned>(
    url: &str,
    token: Option<&str>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut request = ureq::get(url).timeout(Duration::from_secs(20));
    if let Some(token) = token {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = request.call().map_err(|err| match err {
        ureq::Error::Status(404, _) => Box::new(ContentUnavailable) as Box<dyn std::error::Error>,
        other => Box::new(other),
    })?;
    let mut body = String::new();
    response.into_reader().read_to_string(&mut body)?;
    Ok(serde_json::from_str(&body)?)
}

fn read_installed_version(content_dir: &Path) -> Option<String> {
    fs::read_to_string(content_dir.join(CONTENT_VERSION_FILE))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn read_local_manifest(content_dir: &Path) -> Option<Manifest> {
    Manifest::from_json(&fs::read(content_dir.join(CONTENT_MANIFEST_FILE)).ok()?).ok()
}

fn store_manifest_and_version(content_dir: &Path, manifest: &Manifest, version: &str) -> io::Result<()> {
    let json = serde_json::to_vec(manifest)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
    fs::write(content_dir.join(CONTENT_MANIFEST_FILE), json)?;
    fs::write(content_dir.join(CONTENT_VERSION_FILE), version)?;
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let v = bytes as f64;
    if v >= GIB {
        format!("{:.2} GB", v / GIB)
    } else if v >= MIB {
        format!("{:.1} MB", v / MIB)
    } else {
        format!("{:.0} KB", (v / 1024.0).max(1.0))
    }
}

fn gate_landing() -> String {
    match CONTENT_VERSION {
        Some(v) if !v.is_empty() => format!("{GATE}?v={}", urlencoding::encode(v)),
        _ => GATE.to_string(),
    }
}

// -- webview screens ---------------------------------------------------------

fn window_eval(handle: &tauri::AppHandle, js: &str) {
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.eval(js);
    }
}

fn js_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', " ")
}

/// One shared card, matching the content gate exactly: kicker + REBELLION II
/// wordmark top-aligned, an optional spinner, a status line, a progress bar, and
/// buttons pinned toward the bottom. Fixed height so the frame never resizes.
fn render(kicker: &str, spinner: bool, status: &str, buttons: &str) -> String {
    let spin = if spinner { r#"<div class="spin"></div>"# } else { "" };
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8"><style>*{{box-sizing:border-box}}html,body{{height:100%;margin:0}}body{{font-family:"Segoe UI",system-ui,sans-serif;color:#e8ecf6;background:radial-gradient(1200px 800px at 70% -10%,#1a2547 0%,transparent 55%),radial-gradient(900px 700px at 10% 110%,#241238 0%,transparent 50%),linear-gradient(180deg,#0b1226,#05070f);display:flex;align-items:center;justify-content:center;overflow:hidden;user-select:none}}body::before{{content:"";position:fixed;inset:0;background-image:radial-gradient(1.5px 1.5px at 20% 30%,#fff 50%,transparent),radial-gradient(1px 1px at 80% 20%,#cdd 50%,transparent),radial-gradient(1.5px 1.5px at 60% 70%,#fff 50%,transparent),radial-gradient(1px 1px at 35% 80%,#bcd 50%,transparent),radial-gradient(1px 1px at 90% 60%,#fff 50%,transparent),radial-gradient(1.5px 1.5px at 12% 65%,#eef 50%,transparent);opacity:.5;pointer-events:none}}.card{{position:relative;width:min(94vw,440px);height:min(560px,92vh);overflow:hidden;padding:40px 34px 30px;background:rgba(16,22,43,.72);border:1px solid rgba(120,160,255,.18);border-radius:18px;backdrop-filter:blur(14px);box-shadow:0 30px 80px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.05);display:flex;flex-direction:column;text-align:center}}.kicker{{letter-spacing:.42em;font-size:11px;color:#ffcf4d;text-transform:uppercase;margin:0 0 12px;opacity:.9}}h1{{margin:0;font-size:clamp(24px,8vw,40px);font-weight:800;letter-spacing:.1em;line-height:1.05}}h1 .two{{color:#ffcf4d}}.sub{{margin:16px auto 24px;max-width:34ch;color:#8a93ad;font-size:14.5px;line-height:1.6;min-height:20px}}.spin{{width:28px;height:28px;border:2.5px solid rgba(255,255,255,.14);border-top-color:#ffcf4d;border-radius:50%;animation:sp .8s linear infinite;margin:4px auto}}@keyframes sp{{to{{transform:rotate(360deg)}}}}.bar{{width:100%;height:7px;background:rgba(255,255,255,.08);border-radius:5px;overflow:hidden;display:none;margin-top:4px}}.f{{height:100%;width:0%;background:linear-gradient(90deg,#ffcf4d,#ffb43d);transition:width .3s}}.p{{font-size:11.5px;color:#8a93ad;min-height:0;margin-top:8px}}.btns{{margin-top:auto}}.b{{display:flex;align-items:center;justify-content:center;width:100%;padding:14px 18px;margin:11px 0 0;border-radius:11px;font-size:15px;font-weight:700;text-decoration:none;transition:transform .08s ease,filter .15s ease}}.b.primary{{background:#ffcf4d;color:#0a0e1a;box-shadow:0 8px 22px rgba(255,207,77,.22)}}.b.primary:hover{{filter:brightness(1.06);transform:translateY(-1px)}}.b.secondary{{background:rgba(255,255,255,.06);color:#c9d1e6;border:1px solid rgba(255,255,255,.14)}}.b.secondary:hover{{filter:brightness(1.18);transform:translateY(-1px)}}.b.disabled{{background:rgba(255,255,255,.06);color:#5b6479;cursor:default}}</style></head><body><main class="card"><p class="kicker">{kicker}</p><h1>REBELLION <span class="two">II</span></h1><p class="sub" id="s">{status}</p>{spin}<div class="bar" id="bar"><div class="f" id="f"></div></div><div class="p" id="p"></div><div class="btns">{buttons}</div></main><script>window.rebSetProgress=function(p,l){{var b=document.getElementById("bar");if(b)b.style.display="block";var f=document.getElementById("f");if(f)f.style.width=p+"%";var pe=document.getElementById("p");if(pe)pe.textContent=p>0?p+"%":"";if(l){{var s=document.getElementById("s");if(s)s.textContent=l;}}}};window.rebSetStatus=function(l){{var s=document.getElementById("s");if(s)s.textContent=l;}};</script></body></html>"##,
        kicker = kicker,
        status = status,
        spin = spin,
        buttons = buttons,
    )
}

fn button(label: &str, choice: &str) -> String {
    format!("<a class=\"b primary\" href=\"{}?choice={}\">{}</a>", ACT, choice, label)
}

fn disabled_button(label: &str) -> String {
    format!("<span class=\"b disabled\">{}</span>", label)
}

fn write_screen(handle: &tauri::AppHandle, html: &str) {
    let esc = js_escape(html);
    window_eval(handle, &format!("document.open();document.write('{esc}');document.close();"));
}

/// Initial spinner screen \u{2014} Launch Game shown but disabled until the check finishes.
fn show_status_page(handle: &tauri::AppHandle) {
    write_screen(
        handle,
        &render("Checking for updates", true, "Looking for a newer version\u{2026}", &disabled_button("Launch Game")),
    );
}

/// A result screen: kicker + status + one enabled action button.
fn show_result(handle: &tauri::AppHandle, kicker: &str, status: &str, label: &str, choice: &str) {
    write_screen(handle, &render(kicker, false, status, &button(label, choice)));
}

fn show_up_to_date(handle: &tauri::AppHandle, _version: &str) {
    show_result(handle, "Up to date", "You\u{2019}re on the latest version.", "Launch Game", "play");
}

fn show_ready_to_install(handle: &tauri::AppHandle, _version: Option<&str>) {
    show_result(handle, "Ready to install", "Download and install the game to play.", "Install & Launch", "install");
}

/// A plain message screen: kicker + status + an optional single action button.
fn show_message(handle: &tauri::AppHandle, kicker: &str, status: &str, action: Option<(&str, &str)>) {
    let btn = action.map(|(l, c)| button(l, c)).unwrap_or_default();
    write_screen(handle, &render(kicker, false, status, &btn));
}

fn show_progress_ui(handle: &tauri::AppHandle) {
    write_screen(handle, &render("Updating", false, "Preparing update\u{2026}", ""));
    update_progress(handle, 0, "Preparing update\u{2026}");
}

fn show_update_required_ui(handle: &tauri::AppHandle) {
    log_line(&format!("[launcher] update required \u{2014} see {RELEASES_URL}"));
    show_message(
        handle,
        "Update required",
        "This version is no longer supported. Please download the latest build.",
        None,
    );
}

fn update_status(handle: &tauri::AppHandle, label: &str) {
    window_eval(
        handle,
        &format!("window.rebSetStatus&&window.rebSetStatus('{}');", js_escape(label)),
    );
}

fn update_progress(handle: &tauri::AppHandle, percent: u64, label: &str) {
    window_eval(
        handle,
        &format!("window.rebSetProgress&&window.rebSetProgress({percent},'{}');", js_escape(label)),
    );
}

// -- filesystem + process ----------------------------------------------------

fn install_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "launcher has no parent directory"))
}

fn log_line(message: &str) {
    println!("{message}");
    if let Ok(base) = install_dir() {
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(base.join("launcher.log")) {
            let _ = writeln!(file, "{message}");
        }
    }
}

fn install_content(handle: &tauri::AppHandle, url: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = install_dir()?;
    let archive_path = base.join("content.zip.part");
    let content_dir = base.join("Content");

    log_line("[launcher] downloading Content…");
    let response = match ureq::get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Err(Box::new(ContentUnavailable)),
        Err(other) => return Err(Box::new(other)),
    };
    let total: u64 = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = fs::File::create(&archive_path)?;
    let mut buffer = vec![0u8; 1 << 20];
    let gib = (1u64 << 30) as f64;
    let mib = (1u64 << 20) as f64;
    let total_gb = total as f64 / gib;
    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;
    let mut last_time = Instant::now();
    let mut last_bytes: u64 = 0;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        downloaded += read as u64;
        if downloaded - last_report >= 20 * (1 << 20) {
            last_report = downloaded;
            let now = Instant::now();
            let elapsed = now.duration_since(last_time).as_secs_f64();
            let speed = if elapsed > 0.0 { (downloaded - last_bytes) as f64 / mib / elapsed } else { 0.0 };
            last_time = now;
            last_bytes = downloaded;
            let gb = downloaded as f64 / gib;
            let pct = if total > 0 { downloaded * 100 / total } else { 0 };
            let amount = if total > 0 { format!("{gb:.2} / {total_gb:.2} GB") } else { format!("{gb:.2} GB") };
            update_progress(handle, pct, &format!("Downloading Content… {amount} — {speed:.1} MB/s"));
        }
    }
    file.sync_all()?;
    drop(file);
    update_progress(handle, 100, "Extracting Content…");

    if content_dir.exists() {
        fs::remove_dir_all(&content_dir)?;
    }
    fs::create_dir_all(&content_dir)?;
    let mut archive = zip::ZipArchive::new(fs::File::open(&archive_path)?)?;
    archive.extract(&content_dir)?;
    if let Some(version) = CONTENT_VERSION.filter(|v| !v.is_empty()) {
        fs::write(content_dir.join(CONTENT_VERSION_FILE), version)?;
    }
    let _ = fs::remove_file(&archive_path);
    Ok(content_dir)
}

fn launch_game() -> io::Result<bool> {
    let base = install_dir()?;
    let exe = base.join(GAME_EXE);
    if !exe.exists() {
        log_line(&format!("[launcher] game exe not found at {}", exe.display()));
        return Ok(false);
    }
    log_line(&format!("[launcher] launching {}", exe.display()));

    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&exe).spawn()?;

    #[cfg(target_os = "linux")]
    std::process::Command::new(&exe).current_dir(&base).spawn()?;

    // On Windows the launcher runs inside a WebView2 job object that kills child
    // processes when the launcher exits — so a plain spawn dies the moment we
    // close. Break the game out of the job (and detach it) so it keeps running;
    // if the job forbids breakaway, hand off to Explorer, which starts it in its
    // own process tree, independent of ours.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let spawned = std::process::Command::new(&exe)
            .current_dir(&base)
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
            .spawn();
        if let Err(err) = spawned {
            log_line(&format!("[launcher] breakaway spawn failed ({err}); handing off to Explorer."));
            std::process::Command::new("explorer.exe").arg(&exe).spawn()?;
        }
    }
    Ok(true)
}
