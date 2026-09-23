// Rebellion II launcher (Tauri)
//
// Flow:
//   1. Ownership — reuse a cached session token if still valid; otherwise open the
//      ownership service (Steam/GOG/GitHub), which mints a signed token we cache.
//   2. Scan — read the public release pointer and compare its matching application
//      and content versions to what's installed: nothing / behind / current.
//   3. Act — first install or an incremental patch require the user to confirm;
//      "already current" just says so and offers Play.
//
// Content (manifest + blobs) is fetched with the token in an Authorization header,
// so it is never freely downloadable — only the version pointer is public.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
#[cfg(any(target_os = "windows", test))]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

#[cfg(any(target_os = "windows", test))]
use rebellion2_update_core::FileEntry;
#[cfg(target_os = "windows")]
use rebellion2_update_core::Plan;
use rebellion2_update_core::{apply, diff, sha256_hex, BlobSource, Manifest};
use serde::Deserialize;
use tauri::{
    webview::{PageLoadEvent, PageLoadPayload},
    Manager, WebviewUrl, WebviewWindowBuilder,
};
#[cfg(target_os = "macos")]
use tauri_plugin_updater::UpdaterExt;

/// Ownership-service URL injected by release automation (REB2_AUTH_BASE_URL).
const AUTH_BASE: Option<&str> = option_env!("REB2_AUTH_BASE_URL");
/// The content version this launcher was built for (REB2_CONTENT_VERSION).
const CONTENT_VERSION: Option<&str> = option_env!("REB2_CONTENT_VERSION");
/// Public base URL of the release channel (REB2_CONTENT_BASE_URL): holds the public
/// atomic application pointer and token-gated content manifests + blobs.
const CONTENT_BASE: Option<&str> = option_env!("REB2_CONTENT_BASE_URL");

const CONTENT_VERSION_FILE: &str = ".content-version";
const CONTENT_MANIFEST_FILE: &str = ".manifest.json";
#[cfg(target_os = "windows")]
const APPLICATION_MANIFEST_FILE: &str = ".application-manifest.json";
#[cfg(target_os = "windows")]
const APPLICATION_VERSION_FILE: &str = ".application-version";
#[cfg(target_os = "windows")]
const PENDING_APPLICATION_MANIFEST_FILE: &str = ".application-manifest.pending.json";
#[cfg(target_os = "windows")]
const PENDING_APPLICATION_VERSION_FILE: &str = ".application-version.pending";
#[cfg(target_os = "windows")]
const STAGED_LAUNCHER_FILE_NAME: &str = ".rebellion2-launcher.next.exe";
#[cfg(any(target_os = "windows", test))]
const UPDATE_HELPER_FILE_NAME: &str = "rebellion2-update-helper.exe";

#[cfg(any(target_os = "windows", test))]
const LAUNCHER_FILE_NAME: &str = "rebellion2-launcher.exe";
/// Cached ownership session token, stored next to the launcher.
const SESSION_FILE: &str = ".session";

#[cfg(target_os = "windows")]
const GAME_EXE: &str = "Rebellion2.exe";
#[cfg(target_os = "linux")]
const GAME_EXE: &str = "Rebellion2";

#[cfg(target_os = "macos")]
const MACOS_GAME_APP_NAME: &str = "Rebellion2 Game.app";

const RELEASES_URL: &str = "https://github.com/adasgames/rebellion2-installers/releases/latest";

/// Internal URL scheme the in-window buttons navigate to; intercepted in on_nav so
/// the HTML can drive Rust without extra IPC wiring.
const ACT: &str = "https://launcher.invalid/act";

/// The current session token + the pending action, set once we know what to do and
/// read when the user clicks a button in the webview.
static SESSION: Mutex<Option<String>> = Mutex::new(None);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// A newer application release waiting for the user to approve its installation.
#[cfg(target_os = "windows")]
static PENDING_APPLICATION_UPDATE: Mutex<Option<ApplicationUpdate>> = Mutex::new(None);
/// Set when the user continues after an update failure so the content flow can proceed.
static APPLICATION_UPDATE_DISMISSED: AtomicBool = AtomicBool::new(false);
/// Set while ownership verification returns the remote webview to bundled launcher content.
static GATE_RETURN_PENDING: AtomicBool = AtomicBool::new(false);
/// Set until the bundled launcher page is ready for its first channel scan.
static INITIAL_SCAN_PENDING: AtomicBool = AtomicBool::new(false);

/// Ed25519 public key (hex) that must have signed an application manifest.
#[cfg(target_os = "windows")]
const APPLICATION_UPDATE_PUBKEY: &str =
    "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d";

/// The application update and matching content release at `dist/application.json`.
#[derive(Debug, Clone, Deserialize)]
struct ApplicationUpdate {
    #[cfg(any(target_os = "windows", test))]
    version: String,
    #[cfg(any(target_os = "windows", test))]
    manifest: String,
    #[cfg(any(target_os = "windows", test))]
    #[serde(default = "default_application_blobs")]
    blobs: String,
    /// Hex ed25519 signature over the application manifest bytes.
    #[cfg(any(target_os = "windows", test))]
    signature: String,
    /// Content paired with this application release. Older pointers omit it.
    #[serde(default)]
    content: Option<Latest>,
}

#[cfg(any(target_os = "windows", test))]
fn default_application_blobs() -> String {
    "application-blobs/".to_string()
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
        write!(
            f,
            "content for this launcher version is no longer available"
        )
    }
}
impl std::error::Error for ContentUnavailable {}

#[derive(Debug)]
struct AuthorizationRequired;
impl std::fmt::Display for AuthorizationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ownership verification required")
    }
}
impl std::error::Error for AuthorizationRequired {}

/// The content portion of the public release pointer.
#[derive(Debug, Clone, Deserialize)]
struct Latest {
    version: String,
    manifest: String,
    #[serde(default = "default_blobs")]
    blobs: String,
    #[serde(default, rename = "releaseNotes")]
    release_notes: Option<ReleaseNotesPointer>,
}

/// A versioned release-notes document published beside the update manifests.
#[derive(Debug, Clone, Deserialize)]
struct ReleaseNotesPointer {
    path: String,
    sha256: String,
}

/// Release notes shown on the update confirmation screen.
#[derive(Debug, Deserialize)]
struct ReleaseNotes {
    version: String,
    sections: Vec<ReleaseNoteSection>,
}

/// A titled group of release-note items.
#[derive(Debug, Deserialize)]
struct ReleaseNoteSection {
    title: String,
    items: Vec<String>,
}
fn default_blobs() -> String {
    "blobs/".to_string()
}

fn main() {
    #[cfg(target_os = "windows")]
    if hand_off_pending_launcher_update() {
        return;
    }

    // Seed the session token from cache if we have a non-expired one.
    if let Some(token) = read_cached_token() {
        *SESSION.lock().unwrap() = Some(token);
    }

    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.handle().plugin(
                tauri_plugin_updater::Builder::new()
                    .target("macos-universal")
                    .build(),
            )?;

            let handle = app.handle().clone();

            // Ownership gates the DOWNLOAD, never the play. If the game is already
            // installed we go straight to the local screen and let the user launch —
            // sign-in is only needed for a first install (or an explicit --repair).
            let installed = install_dir()
                .map(|d| d.join("Content").join("catalog.xml").is_file())
                .unwrap_or(false);
            let repair = std::env::args().any(|arg| arg == "--repair");
            // A first install or repair needs a fresh presigned Content archive URL
            // from the ownership gate. A cached patch token cannot supply that URL.
            let need_signin = requires_ownership_gate(installed, repair);

            if need_signin {
                // Open the gate; on_nav captures the token from /done, then scans.
                let navigation_handle = handle.clone();
                let page_load_handle = handle.clone();
                WebviewWindowBuilder::new(
                    app,
                    "main",
                    WebviewUrl::External(gate_landing().parse().unwrap()),
                )
                .title("Rebellion 2 Launcher")
                .inner_size(520.0, 700.0)
                .resizable(true)
                .on_navigation(move |url| on_nav(&navigation_handle, url))
                .on_page_load(move |_window, payload| on_page_load(&page_load_handle, &payload))
                .build()?;
            } else {
                // Installed (or already signed in) — go straight to the scan. A scan
                // failure degrades to "launch what's installed", so play never blocks.
                let page_load_handle = handle.clone();
                INITIAL_SCAN_PENDING.store(true, Ordering::Relaxed);
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("Rebellion 2 Launcher")
                    .inner_size(520.0, 700.0)
                    .resizable(true)
                    .on_navigation({
                        let h = handle.clone();
                        move |url| on_nav(&h, url)
                    })
                    .on_page_load(move |_window, payload| on_page_load(&page_load_handle, &payload))
                    .build()?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Rebellion II launcher");
}

// -- navigation interception (gate result + in-window buttons) ----------------

fn on_nav(handle: &tauri::AppHandle, url: &tauri::Url) -> bool {
    let target = url.as_str();
    let auth_base = auth_base();

    // GOG bounces the ?code= via its own page — capture and hand to the gate.
    if target.starts_with("https://embed.gog.com/on_login_success") {
        if let Some(code) = query(url, "code") {
            if let Some(window) = handle.get_webview_window("main") {
                let mut callback = format!(
                    "{auth_base}callback/gog?code={}",
                    urlencoding::encode(&code)
                );
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
    if target.starts_with(&format!("{auth_base}done")) {
        let ok = url.query_pairs().any(|(k, v)| k == "ok" && v == "1");
        let message = query(url, "msg");
        let presigned = query(url, "url");
        let token = query(url, "token");
        on_gate_result(handle, ok, message, presigned, token);
        return false;
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

/// Starts launcher work after bundled content finishes loading.
fn on_page_load(handle: &tauri::AppHandle, payload: &PageLoadPayload<'_>) {
    if payload.event() != PageLoadEvent::Finished || !is_local_app_url(payload.url()) {
        return;
    }

    if GATE_RETURN_PENDING.swap(false, Ordering::Relaxed) {
        resume_after_gate(handle);
    } else if INITIAL_SCAN_PENDING.swap(false, Ordering::Relaxed) {
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
    }
}

/// Returns whether a URL belongs to the bundled Tauri application.
fn is_local_app_url(url: &tauri::Url) -> bool {
    url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost")
}

fn query(url: &tauri::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Returns whether the launcher must obtain a fresh presigned Content archive URL.
fn requires_ownership_gate(installed: bool, repair: bool) -> bool {
    repair || !installed
}

/// Caches successful ownership verification and resumes the operation that requested it.
/// Falls back to the presigned first-install archive when no update is pending.
fn on_gate_result(
    handle: &tauri::AppHandle,
    ok: bool,
    message: Option<String>,
    presigned: Option<String>,
    token: Option<String>,
) {
    if !ok {
        let message = message
            .as_deref()
            .filter(|message| !message.trim().is_empty())
            .unwrap_or("This account does not own either eligible game.");
        log_line("[launcher] Ownership verification was not completed.");
        show_message(
            handle,
            "Not verified",
            message,
            Some(("Try Again", "signin")),
        );
        return;
    }
    log_line("[launcher] VERIFIED via the ownership service.");
    if let Some(token) = token {
        store_token(&token);
        *SESSION.lock().unwrap() = Some(token);
    }
    let pending_update = {
        let pending = PENDING.lock().unwrap();
        match pending.as_ref() {
            Some(Pending::Update { base, latest }) => Some((base.clone(), latest.clone())),
            _ => None,
        }
    };
    // Remember the presigned zip for a possible first install without replacing an
    // update that was waiting for renewed authorization.
    if pending_update.is_none() {
        if let Some(url) = presigned {
            *PENDING.lock().unwrap() = Some(Pending::FirstInstall { url });
        }
    }
    // Return to bundled content before rendering actionable launcher controls. Pages
    // written over the remote ownership origin do not reliably route action links back
    // through Tauri's navigation callback.
    GATE_RETURN_PENDING.store(true, Ordering::Relaxed);
    let handle = handle.clone();
    thread::spawn(move || {
        if !navigate_to_local_app(&handle) {
            GATE_RETURN_PENDING.store(false, Ordering::Relaxed);
            resume_after_gate(&handle);
        }
    });
}

/// Continues the first install or content update that requested ownership verification.
fn resume_after_gate(handle: &tauri::AppHandle) {
    let pending_update = {
        let pending = PENDING.lock().unwrap();
        match pending.as_ref() {
            Some(Pending::Update { base, latest }) => Some((base.clone(), latest.clone())),
            _ => None,
        }
    };

    if let Some((base, latest)) = pending_update {
        show_progress_ui(handle);
        let handle = handle.clone();
        thread::spawn(move || run_update(&handle, &base, &latest));
    } else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
    }
}

/// Navigates the ownership webview back to the bundled launcher page.
fn navigate_to_local_app(handle: &tauri::AppHandle) -> bool {
    let Some(window) = handle.get_webview_window("main") else {
        return false;
    };
    #[cfg(any(target_os = "windows", target_os = "android"))]
    let url = "http://tauri.localhost";
    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    let url = "tauri://localhost";

    match url.parse() {
        Ok(url) => window.navigate(url).is_ok(),
        Err(_) => false,
    }
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
                    show_message(
                        handle,
                        "Couldn't start",
                        "The game failed to start — see launcher.log.",
                        None,
                    );
                }
            }
        }
        "quit" => handle.exit(0),
        "application-update" => start_application_update(handle),
        "skip-application-update" => {
            APPLICATION_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            let handle = handle.clone();
            thread::spawn(move || scan_content_and_prompt(&handle));
        }
        "install" => {
            let pending = PENDING.lock().unwrap().clone();
            match pending {
                Some(Pending::FirstInstall { url }) => start_install(handle.clone(), url),
                _ => {
                    log_line("[launcher] install requested without a first-install URL.");
                    show_message(
                        handle,
                        "Sign in required",
                        "Verify ownership before downloading the game.",
                        Some(("Sign in", "signin")),
                    );
                }
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
        // Return from the gate's sign-in screens to the launcher's own screen.
        "back" => {
            let installed = install_dir()
                .map(|d| d.join("Content").join("catalog.xml").is_file())
                .unwrap_or(false);
            if installed {
                show_status_page(handle);
                let handle = handle.clone();
                thread::spawn(move || scan_and_prompt(&handle));
            } else {
                show_message(
                    handle,
                    "Sign in",
                    "Sign in to verify ownership and download the game.",
                    Some(("Sign in", "signin")),
                );
            }
        }
        // Reopen the ownership gate (sign-in screens).
        "signin" => {
            if let Some(window) = handle.get_webview_window("main") {
                if let Ok(url) = gate_landing().parse() {
                    let _ = window.navigate(url);
                }
            }
        }
        _ => {}
    }
}

// -- application updates -----------------------------------------------------

/// Returns the version of a newer Windows application release, retaining its
/// manifest so the existing handoff helper can install it after approval.
#[cfg(target_os = "windows")]
fn check_application_update(_handle: &tauri::AppHandle) -> Option<String> {
    if APPLICATION_UPDATE_DISMISSED.load(Ordering::Relaxed) {
        return None;
    }
    let base = content_base()?;
    let current = current_application_version()?;
    let update: ApplicationUpdate =
        fetch_json(&format!("{base}dist/application.json"), None).ok()?;
    if !application_release_is_coherent(&update) {
        log_line(&format!(
            "[launcher] refusing application {} because its content release does not match.",
            update.version
        ));
        return None;
    }
    if version_gt(&update.version, &current) {
        let version = update.version.clone();
        *PENDING_APPLICATION_UPDATE.lock().unwrap() = Some(update);
        Some(version)
    } else {
        None
    }
}

/// Checks the independent, signed macOS application channel.
#[cfg(target_os = "macos")]
fn check_application_update(handle: &tauri::AppHandle) -> Option<String> {
    if APPLICATION_UPDATE_DISMISSED.load(Ordering::Relaxed) {
        return None;
    }

    let result = tauri::async_runtime::block_on(async {
        let updater = handle.updater()?;
        updater.check().await
    });
    match result {
        Ok(Some(update)) => Some(update.version),
        Ok(None) => None,
        Err(error) => {
            log_line(&format!(
                "[launcher] macOS application update check failed: {error}"
            ));
            None
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn check_application_update(_handle: &tauri::AppHandle) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn start_application_update(handle: &tauri::AppHandle) {
    let update = PENDING_APPLICATION_UPDATE.lock().unwrap().clone();
    if let Some(update) = update {
        let handle = handle.clone();
        thread::spawn(move || run_windows_application_update(&handle, &update));
    }
}

#[cfg(target_os = "macos")]
fn start_application_update(handle: &tauri::AppHandle) {
    show_progress_ui(handle);
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        run_macos_application_update(&handle).await;
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn start_application_update(_handle: &tauri::AppHandle) {}

#[cfg(target_os = "macos")]
async fn run_macos_application_update(handle: &tauri::AppHandle) {
    update_progress(handle, 0, "Checking application update\u{2026}");
    let updater = match handle.updater() {
        Ok(updater) => updater,
        Err(error) => {
            show_macos_application_update_error(handle, &error);
            return;
        }
    };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            log_line("[launcher] the macOS application update is no longer available.");
            show_message(
                handle,
                "Already current",
                "The application is already up to date.",
                Some(("Continue", "skip-application-update")),
            );
            return;
        }
        Err(error) => {
            show_macos_application_update_error(handle, &error);
            return;
        }
    };

    let mut downloaded = 0_u64;
    let progress_handle = handle.clone();
    let finish_handle = handle.clone();
    let result = update
        .download_and_install(
            move |chunk_size, content_length| {
                downloaded = downloaded.saturating_add(chunk_size as u64);
                let percent = content_length
                    .filter(|total| *total > 0)
                    .map(|total| 5 + downloaded.saturating_mul(90) / total)
                    .unwrap_or(5)
                    .min(95);
                update_progress(
                    &progress_handle,
                    percent,
                    &format!(
                        "Downloading application update\u{2026} {}",
                        human_bytes(downloaded)
                    ),
                );
            },
            move || update_progress(&finish_handle, 96, "Installing application update\u{2026}"),
        )
        .await;

    match result {
        Ok(()) => {
            log_line(&format!(
                "[launcher] macOS application update to {} installed.",
                update.version
            ));
            update_progress(
                handle,
                100,
                "Application update complete. Restarting\u{2026}",
            );
            handle.restart();
        }
        Err(error) => show_macos_application_update_error(handle, &error),
    }
}

#[cfg(target_os = "macos")]
fn show_macos_application_update_error(
    handle: &tauri::AppHandle,
    error: &tauri_plugin_updater::Error,
) {
    log_line(&format!(
        "[launcher] macOS application update failed: {error}"
    ));
    show_message(
        handle,
        "Update failed",
        "The application update failed \u{2014} see launcher.log.",
        Some(("Continue", "skip-application-update")),
    );
}

/// True if dotted version `a` is newer than `b` (numeric per component, missing = 0).
#[cfg(target_os = "windows")]
fn version_gt(a: &str, b: &str) -> bool {
    let parts = |s: &str| {
        s.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Stages application changes, then exits so the helper can promote and relaunch
/// the updated launcher before content scanning resumes.
#[cfg(target_os = "windows")]
fn run_windows_application_update(handle: &tauri::AppHandle, update: &ApplicationUpdate) {
    show_progress_ui(handle);
    update_progress(handle, 0, "Checking application update…");
    match do_application_update(handle, update) {
        Ok(changed) => {
            log_line(&format!(
                "[launcher] application update to {} staged ({changed} files).",
                update.version
            ));
            update_progress(handle, 100, "Application update complete. Restarting…");
            thread::sleep(Duration::from_millis(400));
            handle.exit(0);
        }
        Err(error) => {
            log_line(&format!("[launcher] application update failed: {error}"));
            show_message(
                handle,
                "Update failed",
                "The application update failed — see launcher.log.",
                Some(("Continue", "skip-application-update")),
            );
        }
    }
}

/// Verifies and applies a published application manifest without modifying Content
/// or overwriting the currently running launcher executable.
#[cfg(target_os = "windows")]
fn do_application_update(
    handle: &tauri::AppHandle,
    update: &ApplicationUpdate,
) -> Result<usize, Box<dyn std::error::Error>> {
    let base = content_base().ok_or("application update channel is not configured")?;
    let manifest_bytes = fetch_bytes(&format!("{base}{}", update.manifest))?;
    verify_application_manifest(&manifest_bytes, &update.signature)?;
    let remote = Manifest::from_json(&manifest_bytes)?;
    validate_application_manifest(&remote, &update.version)?;

    let install_dir = install_dir()?;
    let local = read_application_manifest(&install_dir)
        .unwrap_or(snapshot_application_files(&install_dir, &remote)?);
    let plan = diff(Some(&local), &remote);
    update_progress(
        handle,
        5,
        &format!(
            "Downloading application update… {}",
            human_bytes(plan.download_size())
        ),
    );

    let blobs = PublicHttpBlobs {
        base: format!("{base}{}", update.blobs),
    };
    let launcher_entry = plan
        .changed
        .iter()
        .find(|entry| entry.path == LAUNCHER_FILE_NAME)
        .cloned();
    let immediate_plan = Plan {
        changed: plan
            .changed
            .iter()
            .filter(|entry| entry.path != LAUNCHER_FILE_NAME)
            .cloned()
            .collect(),
        removed: plan
            .removed
            .iter()
            .filter(|path| path.as_str() != LAUNCHER_FILE_NAME)
            .cloned()
            .collect(),
    };
    let changed = apply(&immediate_plan, &install_dir, &blobs)?;

    let launcher_changed = launcher_entry.is_some();
    if let Some(entry) = launcher_entry {
        let bytes = blobs.fetch(&entry.sha256)?;
        if sha256_hex(&bytes) != entry.sha256 {
            return Err("staged launcher hash does not match the manifest".into());
        }
        fs::write(install_dir.join(STAGED_LAUNCHER_FILE_NAME), bytes)?;
    }
    fs::write(
        install_dir.join(PENDING_APPLICATION_MANIFEST_FILE),
        manifest_bytes,
    )?;
    fs::write(
        install_dir.join(PENDING_APPLICATION_VERSION_FILE),
        &update.version,
    )?;
    // Relaunch only after the helper promotes the staged launcher and version
    // markers. Content scanning must never continue in the old launcher process.
    start_update_helper(true)?;
    Ok(changed + usize::from(launcher_changed))
}

/// Verifies that an application manifest was signed by the release pipeline.
#[cfg(target_os = "windows")]
fn verify_application_manifest(
    manifest_bytes: &[u8],
    signature: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let key: [u8; 32] = hex::decode(APPLICATION_UPDATE_PUBKEY)?
        .as_slice()
        .try_into()
        .map_err(|_| "bad application-update public key length")?;
    let verifying = VerifyingKey::from_bytes(&key)?;
    let signature = Signature::from_slice(&hex::decode(signature)?)?;
    verifying
        .verify(manifest_bytes, &signature)
        .map_err(|_| "application manifest signature does not match".into())
}

/// Ensures an application manifest cannot modify content, mods, or paths outside
/// the installation directory.
#[cfg(any(target_os = "windows", test))]
fn validate_application_manifest(manifest: &Manifest, version: &str) -> io::Result<()> {
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest version does not match its channel pointer",
        ));
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == LAUNCHER_FILE_NAME)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest does not contain the launcher",
        ));
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "application manifest does not contain the update helper",
        ));
    }
    for entry in &manifest.files {
        let path = Path::new(&entry.path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || is_unmanaged_application_path(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "application manifest contains an unmanaged path",
            ));
        }
    }
    Ok(())
}

/// Returns whether a path belongs to player-managed content rather than the application.
#[cfg(any(target_os = "windows", test))]
fn is_unmanaged_application_path(path: &Path) -> bool {
    let Some(Component::Normal(component)) = path.components().next() else {
        return false;
    };
    let component = component.to_string_lossy();
    component.eq_ignore_ascii_case("Content") || component.eq_ignore_ascii_case("Mods")
}

/// Reads the application manifest installed by setup or the previous update.
#[cfg(target_os = "windows")]
fn read_application_manifest(install_dir: &Path) -> Option<Manifest> {
    let bytes = fs::read(install_dir.join(APPLICATION_MANIFEST_FILE)).ok()?;
    Manifest::from_json(&bytes).ok()
}

/// Builds a safe baseline for installers created before application manifests were
/// shipped by hashing only paths named by the target manifest.
#[cfg(target_os = "windows")]
fn snapshot_application_files(
    install_dir: &Path,
    remote_manifest: &Manifest,
) -> io::Result<Manifest> {
    let mut files = Vec::new();
    for remote_file in &remote_manifest.files {
        let path = install_dir.join(&remote_file.path);
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(path)?;
        files.push(FileEntry {
            path: remote_file.path.clone(),
            sha256: sha256_hex(&bytes),
            size: bytes.len() as u64,
        });
    }
    Ok(Manifest {
        version: "existing".to_string(),
        files,
    })
}

/// Reads the installed application version independently from the content version.
#[cfg(target_os = "windows")]
fn read_application_version() -> Option<String> {
    let install_dir = install_dir().ok()?;
    fs::read_to_string(install_dir.join(APPLICATION_VERSION_FILE))
        .ok()
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .or_else(|| read_application_manifest(&install_dir).map(|manifest| manifest.version))
}

/// A macOS update replaces the app bundle but deliberately preserves downloaded
/// content in Application Support. The bundle's baked version is authoritative.
#[cfg(target_os = "macos")]
fn current_application_version() -> Option<String> {
    CONTENT_VERSION
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

/// Returns the installed application version, falling back to the release baked
/// into launchers that predate the application-version marker.
#[cfg(target_os = "windows")]
fn current_application_version() -> Option<String> {
    read_application_version().or_else(|| {
        CONTENT_VERSION
            .filter(|version| !version.is_empty())
            .map(str::to_string)
    })
}

/// Returns the release baked into platforms without an independent application updater.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn current_application_version() -> Option<String> {
    CONTENT_VERSION
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

/// Content is safe to load only when it was published for this application build.
fn content_matches_application(application_version: Option<&str>, content_version: &str) -> bool {
    application_version == Some(content_version)
}

/// Resolves the immutable content release that belongs to this application.
/// A newer platform release may move the public pointer, but cannot make an older
/// launcher consume media authored for a different application version.
fn content_release_for_application(published: Latest, application_version: Option<&str>) -> Latest {
    match application_version {
        Some(version) if version != published.version => Latest {
            version: version.to_string(),
            manifest: format!("dist/manifest-{version}.json"),
            blobs: default_blobs(),
            release_notes: None,
        },
        _ => published,
    }
}

/// Returns whether embedded content uses the same version as its application release.
#[cfg(any(target_os = "windows", test))]
fn application_release_is_coherent(update: &ApplicationUpdate) -> bool {
    update
        .content
        .as_ref()
        .map(|content| content.version == update.version)
        .unwrap_or(true)
}

/// Downloads a public update-channel object.
#[cfg(target_os = "windows")]
fn fetch_bytes(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    ureq::get(url)
        .call()?
        .into_reader()
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Public application blob source. Manifest signatures authenticate every expected
/// blob hash before this source is used.
#[cfg(target_os = "windows")]
struct PublicHttpBlobs {
    base: String,
}

#[cfg(target_os = "windows")]
impl BlobSource for PublicHttpBlobs {
    fn fetch(&self, sha256: &str) -> io::Result<Vec<u8>> {
        let response = ureq::get(&format!("{}{}", self.base, sha256))
            .call()
            .map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        response.into_reader().read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Starts the installed helper outside the launcher's Windows job so it can wait
/// for this process to exit and then promote the staged launcher.
#[cfg(target_os = "windows")]
fn start_update_helper(relaunch: bool) -> io::Result<()> {
    let install_dir = install_dir()?;
    let helper = install_dir.join(UPDATE_HELPER_FILE_NAME);
    let mut command = std::process::Command::new(helper);
    command.current_dir(&install_dir);
    if relaunch {
        command.arg("--relaunch");
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_BREAKAWAY: u32 = 0x0000_0008 | 0x0100_0000;
        command.creation_flags(DETACHED_BREAKAWAY);
    }
    command.spawn()?;
    Ok(())
}

/// Completes a previously interrupted launcher handoff before any window appears.
#[cfg(target_os = "windows")]
fn hand_off_pending_launcher_update() -> bool {
    let Ok(install_dir) = install_dir() else {
        return false;
    };
    if !install_dir.join(STAGED_LAUNCHER_FILE_NAME).is_file() {
        return false;
    }
    match start_update_helper(true) {
        Ok(()) => true,
        Err(error) => {
            log_line(&format!(
                "[launcher] couldn't resume the staged launcher update: {error}"
            ));
            false
        }
    }
}

// -- scan --------------------------------------------------------------------

/// Reads the channel pointer and decides what to show. Any network failure
/// degrades to "launch what's installed" rather than forcing a re-download.
fn scan_and_prompt(handle: &tauri::AppHandle) {
    update_status(handle, "Checking for updates…");

    // An application update supersedes content, but nothing is downloaded until
    // the user approves it. Discovery failures fall through to the content flow.
    if let Some(version) = check_application_update(handle) {
        log_line(&format!(
            "[launcher] application update available: {version}"
        ));
        show_application_update(handle, &version);
        return;
    }

    scan_content_and_prompt(handle);
}

/// Reads the content channel and offers first install, patching, or launch.
fn scan_content_and_prompt(handle: &tauri::AppHandle) {
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
        Ok(published) => {
            let application_version = current_application_version();
            let latest = content_release_for_application(published, application_version.as_deref());
            // Fail closed before offering either first install or update.
            if !content_matches_application(application_version.as_deref(), &latest.version) {
                let installed_matches_application = content_matches_application(
                    application_version.as_deref(),
                    installed_version.as_deref().unwrap_or_default(),
                );
                log_line(&format!(
                    "[launcher] refusing content {} for application {}.",
                    latest.version,
                    application_version.as_deref().unwrap_or("unknown")
                ));
                if installed_present && installed_matches_application {
                    show_result(
                        handle,
                        "Update pending",
                        "The next release is not fully published yet. Your installed game is safe to play.",
                        "Launch Game",
                        "play",
                    );
                } else if installed_present {
                    show_message(
                        handle,
                        "Repair required",
                        "The installed application and content versions do not match. Reopen the launcher after the release channel is repaired.",
                        None,
                    );
                } else {
                    show_message(
                        handle,
                        "Release unavailable",
                        "The matching game content is not available yet. Reopen the launcher after the release finishes publishing.",
                        None,
                    );
                }
            } else if matches!(*PENDING.lock().unwrap(), Some(Pending::FirstInstall { .. })) {
                show_ready_to_install(handle, Some(&latest.version));
            } else if !installed_present {
                log_line("[launcher] first install requires ownership verification.");
                show_message(
                    handle,
                    "Sign in required",
                    "Verify ownership before downloading the game.",
                    Some(("Sign in", "signin")),
                );
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
            log_line(&format!(
                "[launcher] update check failed ({err}); offering to play installed."
            ));
            if installed_present {
                show_up_to_date(handle, installed_version.as_deref().unwrap_or("installed"));
            }
        }
    }
}

/// Requests authorization when necessary, then computes the update size and prompts.
fn prompt_update(handle: &tauri::AppHandle, base: &str, latest: &Latest, content_dir: &Path) {
    let token = SESSION.lock().unwrap().clone();
    *PENDING.lock().unwrap() = Some(Pending::Update {
        base: base.to_string(),
        latest: latest.clone(),
    });

    let Some(token) = token else {
        show_update_signin(handle);
        return;
    };
    let remote: Option<Manifest> =
        match fetch_json(&format!("{base}{}", latest.manifest), Some(&token)) {
            Ok(remote) => Some(remote),
            Err(err) if is_authorization_required(err.as_ref()) => {
                clear_token();
                show_update_signin(handle);
                return;
            }
            Err(_) => None,
        };
    let bytes = match &remote {
        Some(remote) => {
            let local = read_local_manifest(content_dir).or_else(|| {
                read_installed_version(content_dir).and_then(|v| {
                    fetch_json::<Manifest>(&format!("{base}dist/manifest-{v}.json"), Some(&token))
                        .ok()
                })
            });
            let plan = diff(local.as_ref(), remote);
            plan.download_size()
        }
        None => 0,
    };

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
    let release_notes = fetch_release_notes(base, latest);
    write_screen(
        handle,
        &update_available_screen(&detail, release_notes.as_ref(), &buttons),
    );
}

/// Builds the content-update confirmation screen with optional release notes.
fn update_available_screen(detail: &str, notes: Option<&ReleaseNotes>, buttons: &str) -> String {
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content("Update available", false, detail, &release_notes, buttons)
}

/// Downloads and validates the notes referenced by a content release.
fn fetch_release_notes(base: &str, latest: &Latest) -> Option<ReleaseNotes> {
    let pointer = latest.release_notes.as_ref()?;
    let bytes = fetch_channel_bytes(&format!("{base}{}", pointer.path), None).ok()?;
    parse_release_notes(&bytes, &latest.version, &pointer.sha256)
}

/// Parses release notes only when their version and digest match the release pointer.
fn parse_release_notes(bytes: &[u8], version: &str, sha256: &str) -> Option<ReleaseNotes> {
    if sha256_hex(bytes) != sha256 {
        return None;
    }

    let notes = serde_json::from_slice::<ReleaseNotes>(bytes).ok()?;
    if notes.version != version
        || notes.sections.is_empty()
        || notes
            .sections
            .iter()
            .any(|section| section.title.trim().is_empty() || section.items.is_empty())
    {
        return None;
    }

    Some(notes)
}

/// Renders structured release notes for the update confirmation screen.
fn render_release_notes(notes: &ReleaseNotes) -> String {
    let sections = notes
        .sections
        .iter()
        .map(|section| {
            let items = section
                .items
                .iter()
                .map(|item| format!("<li>{}</li>", html_escape(item)))
                .collect::<String>();
            format!(
                "<section class=\"changes\"><h2 style=\"font-size:11px;font-weight:800\">{}</h2><ul style=\"font-size:12px\">{items}</ul></section>",
                html_escape(&section.title)
            )
        })
        .collect::<String>();

    format!(
        "<div style=\"display:flex;flex:1;min-height:0;margin-top:16px;padding-top:14px;border-top:1px solid rgba(255,255,255,.12);flex-direction:column;text-align:left\"><h2 class=\"patch-title\" style=\"font-size:15px\">Patch Notes</h2><div class=\"release-changes\" style=\"flex:1;overflow-y:scroll;scrollbar-gutter:stable\">{sections}</div></div>"
    )
}

// -- update / install work ---------------------------------------------------

fn run_update(handle: &tauri::AppHandle, base: &str, latest: &Latest) {
    match do_update(handle, base, latest) {
        Ok(fetched) => {
            log_line(&format!(
                "[launcher] update to {} complete ({fetched} files).",
                latest.version
            ));
            update_progress(handle, 100, "Update complete.");
            // Let the filled bar sit a beat, then land on the up-to-date screen.
            thread::sleep(Duration::from_millis(900));
            show_up_to_date(handle, &latest.version);
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
            // A missing manifest/blob during an update means the channel is
            // unreachable or misconfigured — NOT that this version is retired.
            log_line("[launcher] update content unavailable (404 from channel).");
            update_progress(
                handle,
                0,
                "Update failed — content unavailable. Please try again later.",
            );
        }
        Err(err) if is_authorization_required(err.as_ref()) => {
            log_line("[launcher] update authorization expired; requesting ownership verification.");
            clear_token();
            show_update_signin(handle);
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
    update_progress(
        handle,
        5,
        &format!("Downloading {changed} files, {}", human_bytes(bytes)),
    );

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
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    AuthorizationRequired,
                ))
            }
            Err(other) => return Err(io::Error::other(other.to_string())),
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
            let percent = done
                .saturating_mul(90)
                .checked_div(self.total)
                .map(|percent| (5 + percent).min(95))
                .unwrap_or(50);
            if percent != last_percent {
                last_percent = percent;
                update_progress(
                    &self.handle,
                    percent,
                    &format!(
                        "Downloading update… {} / {}",
                        human_bytes(done),
                        human_bytes(self.total)
                    ),
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
            log_line(&format!(
                "[launcher] Content installed to {}",
                content_dir.display()
            ));
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
                Ok(false) => show_message(
                    &handle,
                    "Installed",
                    "The game is ready to play.",
                    Some(("Launch Game", "play")),
                ),
                Err(err) => log_line(&format!("[launcher] failed to start the game: {err}")),
            }
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
            show_update_required_ui(&handle)
        }
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

fn clear_token() {
    *SESSION.lock().unwrap() = None;
    if let Ok(base) = install_dir() {
        let path = base.join(SESSION_FILE);
        if let Err(err) = fs::remove_file(path) {
            if err.kind() != io::ErrorKind::NotFound {
                log_line(&format!(
                    "[launcher] couldn't remove the expired session: {err}"
                ));
            }
        }
    }
}

fn is_authorization_required(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<AuthorizationRequired>().is_some()
        || error
            .downcast_ref::<io::Error>()
            .and_then(|error| error.get_ref())
            .is_some_and(|error| error.downcast_ref::<AuthorizationRequired>().is_some())
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
        .map(|v| {
            if v.ends_with('/') {
                v.to_string()
            } else {
                format!("{v}/")
            }
        })
}

fn auth_base() -> String {
    let base = AUTH_BASE
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("http://127.0.0.1:8787/");
    format!("{}/", base.trim_end_matches('/'))
}

fn fetch_latest(base: &str) -> Result<Latest, Box<dyn std::error::Error>> {
    if let Ok(release) =
        fetch_json::<ApplicationUpdate>(&format!("{base}dist/application.json"), None)
    {
        if let Some(content) = release.content {
            return Ok(content);
        }
    }

    // Launchers released before the atomic channel migration still depend on
    // latest.json. It remains pinned to their last compatible content version.
    fetch_json(&format!("{base}dist/latest.json"), None)
}

fn fetch_json<T: serde::de::DeserializeOwned>(
    url: &str,
    token: Option<&str>,
) -> Result<T, Box<dyn std::error::Error>> {
    let body = fetch_channel_bytes(url, token)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Fetches bytes from the release channel with optional ownership authorization.
fn fetch_channel_bytes(
    url: &str,
    token: Option<&str>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut request = ureq::get(url).timeout(Duration::from_secs(20));
    if let Some(token) = token {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = request.call().map_err(|err| match err {
        ureq::Error::Status(404, _) => Box::new(ContentUnavailable) as Box<dyn std::error::Error>,
        ureq::Error::Status(401 | 403, _) => {
            Box::new(AuthorizationRequired) as Box<dyn std::error::Error>
        }
        other => Box::new(other),
    })?;
    let mut body = Vec::new();
    response.into_reader().read_to_end(&mut body)?;
    Ok(body)
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

fn store_manifest_and_version(
    content_dir: &Path,
    manifest: &Manifest,
    version: &str,
) -> io::Result<()> {
    let json = serde_json::to_vec(manifest).map_err(|err| io::Error::other(err.to_string()))?;
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
    let auth_base = auth_base();
    match CONTENT_VERSION {
        Some(v) if !v.is_empty() => format!("{auth_base}?v={}", urlencoding::encode(v)),
        _ => auth_base,
    }
}

// -- webview screens ---------------------------------------------------------

fn window_eval(handle: &tauri::AppHandle, js: &str) {
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.eval(js);
    }
}

fn js_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// One shared card, matching the ownership page exactly: kicker + REBELLION II
/// wordmark top-aligned, an optional spinner, a status line, a progress bar, and
/// buttons pinned toward the bottom. Fixed height so the frame never resizes.
fn render(kicker: &str, spinner: bool, status: &str, buttons: &str) -> String {
    render_with_content(kicker, spinner, status, "", buttons)
}

/// Renders the shared launcher card with optional content above its actions.
fn render_with_content(
    kicker: &str,
    spinner: bool,
    status: &str,
    content: &str,
    buttons: &str,
) -> String {
    let spin = if spinner {
        r#"<div class="spin"></div>"#
    } else {
        ""
    };
    let kicker = html_escape(kicker);
    let status = html_escape(status);
    let card_class = if content.is_empty() {
        "card"
    } else {
        "card has-content"
    };
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8"><style>*{{box-sizing:border-box}}html,body{{height:100%;margin:0}}body{{font-family:"Segoe UI",system-ui,sans-serif;color:#e8ecf6;background:radial-gradient(1200px 800px at 70% -10%,#1a2547 0%,transparent 55%),radial-gradient(900px 700px at 10% 110%,#241238 0%,transparent 50%),linear-gradient(180deg,#0b1226,#05070f);display:flex;align-items:center;justify-content:center;overflow:hidden;user-select:none}}body::before{{content:"";position:fixed;inset:0;background-image:radial-gradient(1.5px 1.5px at 20% 30%,#fff 50%,transparent),radial-gradient(1px 1px at 80% 20%,#cdd 50%,transparent),radial-gradient(1.5px 1.5px at 60% 70%,#fff 50%,transparent),radial-gradient(1px 1px at 35% 80%,#bcd 50%,transparent),radial-gradient(1px 1px at 90% 60%,#fff 50%,transparent),radial-gradient(1.5px 1.5px at 12% 65%,#eef 50%,transparent);opacity:.5;pointer-events:none}}.card{{position:relative;width:min(94vw,440px);height:min(560px,92vh);overflow:hidden;padding:40px 34px 30px;background:rgba(16,22,43,.72);border:1px solid rgba(120,160,255,.18);border-radius:18px;backdrop-filter:blur(14px);box-shadow:0 30px 80px rgba(0,0,0,.55),inset 0 1px 0 rgba(255,255,255,.05);display:flex;flex-direction:column;text-align:center}}.kicker{{letter-spacing:.42em;font-size:11px;color:#ffcf4d;text-transform:uppercase;margin:0 0 12px;opacity:.9}}h1{{margin:0;font-size:clamp(24px,8vw,40px);font-weight:800;letter-spacing:.1em;line-height:1.05}}h1 .two{{color:#ffcf4d}}.sub{{margin:16px auto 18px;max-width:34ch;color:#8a93ad;font-size:14.5px;line-height:1.6;min-height:20px}}.has-content .sub{{margin-bottom:0}}.spin{{width:28px;height:28px;border:2.5px solid rgba(255,255,255,.14);border-top-color:#ffcf4d;border-radius:50%;animation:sp .8s linear infinite;margin:4px auto}}@keyframes sp{{to{{transform:rotate(360deg)}}}}.bar{{width:100%;height:7px;background:rgba(255,255,255,.08);border-radius:5px;overflow:hidden;display:none;margin-top:4px}}.f{{height:100%;width:0%;background:linear-gradient(90deg,#ffcf4d,#ffb43d);transition:width .3s}}.p{{font-size:11.5px;color:#8a93ad;min-height:0;margin-top:8px}}.has-content .p:empty{{margin:0}}.release-changes{{min-height:0;overflow-y:auto;padding-right:8px;text-align:left;scrollbar-color:#59637b rgba(255,255,255,.06);scrollbar-width:thin}}.release-changes::-webkit-scrollbar{{width:7px}}.release-changes::-webkit-scrollbar-track{{background:rgba(255,255,255,.06);border-radius:4px}}.release-changes::-webkit-scrollbar-thumb{{background:#59637b;border-radius:4px}}.patch-title{{margin:0 0 9px;color:#e8ecf6;font-size:13px}}.save-warning{{margin:0 0 16px;color:#ff3434;font-size:12px;font-weight:900;line-height:1.25;text-align:center;text-transform:uppercase;letter-spacing:.025em}}.changes+.changes{{margin-top:9px}}.changes h2{{margin:0 0 3px;color:#b9c2da;font-size:9px;text-transform:uppercase}}.changes ul{{margin:0;padding-left:18px;color:#cbd2e3;font-size:11px;line-height:1.45}}.changes li+li{{margin-top:5px}}.btns{{flex:none;margin-top:0}}.b{{display:flex;align-items:center;justify-content:center;width:100%;padding:14px 18px;margin:11px 0 0;border-radius:11px;font-size:15px;font-weight:700;text-decoration:none;transition:transform .08s ease,filter .15s ease}}.b.primary{{background:#ffcf4d;color:#0a0e1a;box-shadow:0 8px 22px rgba(255,207,77,.22)}}.b.primary:hover{{filter:brightness(1.06);transform:translateY(-1px)}}.b.secondary{{background:rgba(255,255,255,.06);color:#c9d1e6;border:1px solid rgba(255,255,255,.14)}}.b.secondary:hover{{filter:brightness(1.18);transform:translateY(-1px)}}.b.disabled{{background:rgba(255,255,255,.06);color:#5b6479;cursor:default}}</style></head><body><main class="{card_class}"><p class="kicker">{kicker}</p><h1>REBELLION <span class="two">II</span></h1><p class="sub" id="s">{status}</p>{spin}<div class="bar" id="bar"><div class="f" id="f"></div></div><div class="p" id="p"></div>{content}<div class="btns">{buttons}</div></main><script>window.rebSetProgress=function(p,l){{var b=document.getElementById("bar");if(b)b.style.display="block";var f=document.getElementById("f");if(f)f.style.width=p+"%";var pe=document.getElementById("p");if(pe)pe.textContent=p>0?p+"%":"";if(l){{var s=document.getElementById("s");if(s)s.textContent=l;}}}};window.rebSetStatus=function(l){{var s=document.getElementById("s");if(s)s.textContent=l;}};</script></body></html>"##,
        kicker = kicker,
        status = status,
        spin = spin,
        content = content,
        card_class = card_class,
        buttons = buttons,
    )
}

fn button(label: &str, choice: &str) -> String {
    format!(
        "<a class=\"b primary\" href=\"{}?choice={}\">{}</a>",
        ACT, choice, label
    )
}

fn disabled_button(label: &str) -> String {
    format!("<span class=\"b disabled\">{}</span>", label)
}

fn write_screen(handle: &tauri::AppHandle, html: &str) {
    let esc = js_escape(html);
    window_eval(
        handle,
        &format!("document.open();document.write('{esc}');document.close();"),
    );
}

/// Initial spinner screen \u{2014} Launch Game shown but disabled until the check finishes.
fn show_status_page(handle: &tauri::AppHandle) {
    write_screen(
        handle,
        &render(
            "Checking for updates",
            true,
            "Looking for a newer version\u{2026}",
            &disabled_button("Launch Game"),
        ),
    );
}

/// A result screen: kicker + status + one enabled action button.
fn show_result(handle: &tauri::AppHandle, kicker: &str, status: &str, label: &str, choice: &str) {
    write_screen(
        handle,
        &render(kicker, false, status, &button(label, choice)),
    );
}

fn show_up_to_date(handle: &tauri::AppHandle, _version: &str) {
    show_result(
        handle,
        "Up to date",
        "You\u{2019}re on the latest version.",
        "Launch Game",
        "play",
    );
}

fn show_ready_to_install(handle: &tauri::AppHandle, _version: Option<&str>) {
    show_result(
        handle,
        "Ready to install",
        "Download and install the game to play.",
        "Install & Launch",
        "install",
    );
}

/// Offers an available application update without beginning its download.
fn show_application_update(handle: &tauri::AppHandle, version: &str) {
    write_screen(handle, &application_update_screen(version));
}

/// Builds the confirmation screen shown before an application update downloads.
fn application_update_screen(version: &str) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=application-update\">Install Update</a>\
         <a class=\"b secondary\" href=\"{a}?choice=skip-application-update\">Not Now</a>",
        a = ACT,
    );
    render(
        "Update available",
        false,
        &format!("Version {version} is available. Install it now?"),
        &buttons,
    )
}

fn show_update_signin(handle: &tauri::AppHandle) {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=signin\">Sign in &amp; Update</a>\
         <a class=\"b secondary\" href=\"{a}?choice=play\">Launch Game</a>",
        a = ACT,
    );
    write_screen(
        handle,
        &render(
            "Sign in required",
            false,
            "Verify ownership to download this update.",
            &buttons,
        ),
    );
}

/// A plain message screen: kicker + status + an optional single action button.
fn show_message(
    handle: &tauri::AppHandle,
    kicker: &str,
    status: &str,
    action: Option<(&str, &str)>,
) {
    let btn = action.map(|(l, c)| button(l, c)).unwrap_or_default();
    write_screen(handle, &render(kicker, false, status, &btn));
}

fn show_progress_ui(handle: &tauri::AppHandle) {
    write_screen(
        handle,
        &render("Updating", false, "Preparing update\u{2026}", ""),
    );
    update_progress(handle, 0, "Preparing update\u{2026}");
}

fn show_update_required_ui(handle: &tauri::AppHandle) {
    log_line(&format!(
        "[launcher] update required \u{2014} see {RELEASES_URL}"
    ));
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
        &format!(
            "window.rebSetStatus&&window.rebSetStatus('{}');",
            js_escape(label)
        ),
    );
}

fn update_progress(handle: &tauri::AppHandle, percent: u64, label: &str) {
    window_eval(
        handle,
        &format!(
            "window.rebSetProgress&&window.rebSetProgress({percent},'{}');",
            js_escape(label)
        ),
    );
}

// -- filesystem + process ----------------------------------------------------

#[cfg(not(target_os = "macos"))]
fn install_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "launcher has no parent directory"))
}

#[cfg(target_os = "macos")]
fn install_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not configured"))?;
    let directory = macos_data_dir(&home);
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

#[cfg(any(target_os = "macos", test))]
fn macos_data_dir(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Rebellion 2")
}

#[cfg(any(target_os = "macos", test))]
fn macos_bundle_contents_dir(executable: &Path) -> io::Result<PathBuf> {
    let macos_directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "launcher executable has no parent directory",
        )
    })?;
    let contents_directory = macos_directory.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "launcher bundle has no Contents directory",
        )
    })?;
    if macos_directory.file_name().and_then(|name| name.to_str()) != Some("MacOS")
        || contents_directory
            .file_name()
            .and_then(|name| name.to_str())
            != Some("Contents")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher is not running from a macOS app bundle",
        ));
    }
    Ok(contents_directory.to_path_buf())
}

#[cfg(target_os = "macos")]
fn game_path() -> io::Result<PathBuf> {
    Ok(macos_bundle_contents_dir(&std::env::current_exe()?)?
        .join("Resources")
        .join(MACOS_GAME_APP_NAME))
}

#[cfg(not(target_os = "macos"))]
fn game_path() -> io::Result<PathBuf> {
    Ok(install_dir()?.join(GAME_EXE))
}

fn log_line(message: &str) {
    println!("{message}");
    if let Ok(base) = install_dir() {
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("launcher.log"))
        {
            let _ = writeln!(file, "{message}");
        }
    }
}

fn install_content(
    handle: &tauri::AppHandle,
    url: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
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
            let speed = if elapsed > 0.0 {
                (downloaded - last_bytes) as f64 / mib / elapsed
            } else {
                0.0
            };
            last_time = now;
            last_bytes = downloaded;
            let gb = downloaded as f64 / gib;
            let pct = downloaded
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0);
            let amount = if total > 0 {
                format!("{gb:.2} / {total_gb:.2} GB")
            } else {
                format!("{gb:.2} GB")
            };
            update_progress(
                handle,
                pct,
                &format!("Downloading Content… {amount} — {speed:.1} MB/s"),
            );
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
    let exe = game_path()?;
    if !exe.exists() {
        log_line(&format!(
            "[launcher] game exe not found at {}",
            exe.display()
        ));
        return Ok(false);
    }
    log_line(&format!("[launcher] launching {}", exe.display()));

    #[cfg(target_os = "macos")]
    std::process::Command::new("/usr/bin/open")
        .arg(&exe)
        .arg("--args")
        .arg("-contentPath")
        .arg(base.join("Content"))
        .spawn()?;

    #[cfg(target_os = "linux")]
    std::process::Command::new(&exe)
        .current_dir(&base)
        .spawn()?;

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
            log_line(&format!(
                "[launcher] breakaway spawn failed ({err}); handing off to Explorer."
            ));
            std::process::Command::new("explorer.exe")
                .arg(&exe)
                .spawn()?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn application_manifest(version: &str, paths: &[&str]) -> Manifest {
        Manifest {
            version: version.to_string(),
            files: paths
                .iter()
                .map(|path| FileEntry {
                    path: (*path).to_string(),
                    sha256: sha256_hex(path.as_bytes()),
                    size: path.len() as u64,
                })
                .collect(),
        }
    }

    #[test]
    fn requires_ownership_gate_without_installed_content_returns_true() {
        assert!(requires_ownership_gate(false, false));
    }

    #[test]
    fn requires_ownership_gate_for_repair_returns_true() {
        assert!(requires_ownership_gate(true, true));
    }

    #[test]
    fn requires_ownership_gate_with_installed_content_returns_false() {
        assert!(!requires_ownership_gate(true, false));
    }

    #[test]
    fn content_matches_application_with_same_version_returns_true() {
        assert!(content_matches_application(Some("0.0.11"), "0.0.11"));
    }

    #[test]
    fn content_matches_application_with_different_version_returns_false() {
        assert!(!content_matches_application(Some("0.0.9"), "0.0.10"));
    }

    #[test]
    fn content_matches_application_without_application_version_returns_false() {
        assert!(!content_matches_application(None, "0.0.11"));
    }

    #[test]
    fn content_release_for_application_with_matching_version_keeps_published_pointer() {
        let published = Latest {
            version: "0.0.14".to_string(),
            manifest: "dist/custom-manifest.json".to_string(),
            blobs: "custom-blobs/".to_string(),
            release_notes: Some(ReleaseNotesPointer {
                path: "dist/release-notes-0.0.14.json".to_string(),
                sha256: "digest".to_string(),
            }),
        };

        let resolved = content_release_for_application(published, Some("0.0.14"));

        assert_eq!(resolved.version, "0.0.14");
        assert_eq!(resolved.manifest, "dist/custom-manifest.json");
        assert_eq!(resolved.blobs, "custom-blobs/");
        assert!(resolved.release_notes.is_some());
    }

    #[test]
    fn content_release_for_application_with_newer_published_version_uses_immutable_matching_content(
    ) {
        let published = Latest {
            version: "0.0.15".to_string(),
            manifest: "dist/manifest-0.0.15.json".to_string(),
            blobs: "blobs/".to_string(),
            release_notes: Some(ReleaseNotesPointer {
                path: "dist/release-notes-0.0.15.json".to_string(),
                sha256: "digest".to_string(),
            }),
        };

        let resolved = content_release_for_application(published, Some("0.0.14"));

        assert_eq!(resolved.version, "0.0.14");
        assert_eq!(resolved.manifest, "dist/manifest-0.0.14.json");
        assert_eq!(resolved.blobs, "blobs/");
        assert!(resolved.release_notes.is_none());
    }

    #[test]
    fn content_release_for_application_without_application_version_keeps_published_pointer() {
        let published = Latest {
            version: "0.0.14".to_string(),
            manifest: "dist/manifest-0.0.14.json".to_string(),
            blobs: "blobs/".to_string(),
            release_notes: None,
        };

        let resolved = content_release_for_application(published, None);

        assert_eq!(resolved.version, "0.0.14");
    }

    #[test]
    fn application_update_with_embedded_content_keeps_versions_together() {
        let release: ApplicationUpdate = serde_json::from_str(
            r#"{
                "version":"0.0.11",
                "manifest":"dist/application-manifest-0.0.11.json",
                "blobs":"application-blobs/",
                "signature":"signed",
                "content":{
                    "version":"0.0.11",
                    "manifest":"dist/manifest-0.0.11.json",
                    "blobs":"blobs/"
                }
            }"#,
        )
        .unwrap();

        assert_eq!(release.manifest, "dist/application-manifest-0.0.11.json");
        assert_eq!(release.blobs, "application-blobs/");
        assert_eq!(release.signature, "signed");
        assert_eq!(release.content.as_ref().unwrap().version, "0.0.11");
        assert!(application_release_is_coherent(&release));
    }

    #[test]
    fn application_update_with_mismatched_content_is_incoherent() {
        let release: ApplicationUpdate = serde_json::from_str(
            r#"{
                "version":"0.0.11",
                "manifest":"dist/application-manifest-0.0.11.json",
                "blobs":"application-blobs/",
                "signature":"signed",
                "content":{
                    "version":"0.0.10",
                    "manifest":"dist/manifest-0.0.10.json",
                    "blobs":"blobs/"
                }
            }"#,
        )
        .unwrap();

        assert!(!application_release_is_coherent(&release));
    }

    #[test]
    fn legacy_application_update_without_content_remains_readable() {
        let release: ApplicationUpdate = serde_json::from_str(
            r#"{
                "version":"0.0.9",
                "manifest":"dist/application-manifest-0.0.9.json",
                "blobs":"application-blobs/",
                "signature":"signed"
            }"#,
        )
        .unwrap();

        assert!(release.content.is_none());
        assert!(application_release_is_coherent(&release));
    }

    #[test]
    fn is_local_app_url_with_tauri_scheme_returns_true() {
        let url = tauri::Url::parse("tauri://localhost").unwrap();

        assert!(is_local_app_url(&url));
    }

    #[test]
    fn is_local_app_url_with_windows_tauri_host_returns_true() {
        let url = tauri::Url::parse("http://tauri.localhost").unwrap();

        assert!(is_local_app_url(&url));
    }

    #[test]
    fn is_local_app_url_with_remote_host_returns_false() {
        let url = tauri::Url::parse("https://example.invalid/done").unwrap();

        assert!(!is_local_app_url(&url));
    }

    #[test]
    fn is_authorization_required_with_authorization_error_returns_true() {
        let error = AuthorizationRequired;

        assert!(is_authorization_required(&error));
    }

    #[test]
    fn is_authorization_required_with_wrapped_authorization_error_returns_true() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, AuthorizationRequired);

        assert!(is_authorization_required(&error));
    }

    #[test]
    fn is_authorization_required_with_filesystem_permission_error_returns_false() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "read-only file");

        assert!(!is_authorization_required(&error));
    }

    #[test]
    fn application_update_screen_with_available_update_offers_install_and_skip() {
        let screen = application_update_screen("1.2.3");

        assert!(screen.contains("Version 1.2.3 is available. Install it now?"));
        assert!(screen.contains("choice=application-update\">Install Update"));
        assert!(screen.contains("choice=skip-application-update\">Not Now"));
    }

    #[test]
    fn update_available_screen_with_release_notes_displays_release_content() {
        let notes = ReleaseNotes {
            version: "1.2.3".to_string(),
            sections: vec![ReleaseNoteSection {
                title: "Fixes".to_string(),
                items: vec!["Fixed update handling.".to_string()],
            }],
        };
        let screen = update_available_screen(
            "An update is available.",
            Some(&notes),
            "<a href=\"https://launcher.invalid/act?choice=update\">Update</a>",
        );

        assert!(screen.contains("Fixed update handling."));
        assert!(screen.contains("Patch Notes"));
        assert!(screen.contains("Fixes"));
        assert!(screen.contains("choice=update"));
    }

    #[test]
    fn update_available_screen_without_release_notes_omits_release_content() {
        let screen = update_available_screen("An update is available.", None, "Update");

        assert!(!screen.contains("Patch Notes"));
        assert!(screen.contains("An update is available."));
    }

    #[test]
    fn parse_release_notes_with_malformed_json_returns_none() {
        let bytes = br#"{"version":"1.2.3","sections":[}"#;

        assert!(parse_release_notes(bytes, "1.2.3", &sha256_hex(bytes)).is_none());
    }

    #[test]
    fn parse_release_notes_with_wrong_digest_returns_none() {
        let bytes = br#"{"version":"1.2.3","sections":[]}"#;

        assert!(parse_release_notes(bytes, "1.2.3", "incorrect").is_none());
    }

    #[test]
    fn render_escapes_status_text() {
        let screen = render("Status", false, "<script>alert('gate')</script>", "");

        assert!(screen.contains("&lt;script&gt;alert(&#39;gate&#39;)&lt;/script&gt;"));
        assert!(!screen.contains("<script>alert('gate')</script>"));
    }

    #[test]
    fn validate_application_manifest_with_managed_paths_returns_ok() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                LAUNCHER_FILE_NAME,
                UPDATE_HELPER_FILE_NAME,
                "Rebellion2.exe",
            ],
        );

        assert!(validate_application_manifest(&manifest, "1.2.3").is_ok());
    }

    #[test]
    fn validate_application_manifest_with_different_version_returns_error() {
        let manifest = application_manifest("1.2.3", &[LAUNCHER_FILE_NAME]);

        assert!(validate_application_manifest(&manifest, "1.2.4").is_err());
    }

    #[test]
    fn validate_application_manifest_without_launcher_returns_error() {
        let manifest = application_manifest("1.2.3", &["Rebellion2.exe"]);

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn validate_application_manifest_with_content_path_returns_error() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                LAUNCHER_FILE_NAME,
                UPDATE_HELPER_FILE_NAME,
                "Content/catalog.xml",
            ],
        );

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn validate_application_manifest_with_lowercase_content_path_returns_error() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                LAUNCHER_FILE_NAME,
                UPDATE_HELPER_FILE_NAME,
                "content/catalog.xml",
            ],
        );

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn validate_application_manifest_without_update_helper_returns_error() {
        let manifest = application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, "Rebellion2.exe"]);

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn validate_application_manifest_with_parent_traversal_returns_error() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                LAUNCHER_FILE_NAME,
                UPDATE_HELPER_FILE_NAME,
                "../outside.txt",
            ],
        );

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn macos_data_dir_uses_application_support() {
        assert_eq!(
            macos_data_dir(Path::new("/Users/player")),
            Path::new("/Users/player/Library/Application Support/Rebellion 2")
        );
    }

    #[test]
    fn macos_bundle_contents_dir_returns_contents_directory() {
        let executable =
            Path::new("/Applications/Rebellion II.app/Contents/MacOS/rebellion2-launcher");

        assert_eq!(
            macos_bundle_contents_dir(executable).unwrap(),
            Path::new("/Applications/Rebellion II.app/Contents")
        );
    }

    #[test]
    fn macos_bundle_contents_dir_rejects_unbundled_executable() {
        assert!(macos_bundle_contents_dir(Path::new("/tmp/rebellion2-launcher")).is_err());
    }
}
