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

use std::cmp::Ordering as VersionOrdering;
use std::io::{Read, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

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
/// The independently released launcher version (REB2_LAUNCHER_VERSION).
const LAUNCHER_VERSION: Option<&str> = option_env!("REB2_LAUNCHER_VERSION");
/// The content version this launcher was built for (REB2_CONTENT_VERSION).
/// This remains only as a compatibility fallback for bootstrap installers.
const CONTENT_VERSION: Option<&str> = option_env!("REB2_CONTENT_VERSION");
/// Public base URL of the release channel (REB2_CONTENT_BASE_URL): holds the public
/// atomic application pointer and token-gated content manifests + blobs.
const CONTENT_BASE: Option<&str> = option_env!("REB2_CONTENT_BASE_URL");

const CONTENT_VERSION_FILE: &str = ".content-version";
const CONTENT_MANIFEST_FILE: &str = ".manifest.json";
const APPROVED_CONTENT_UPDATE_FILE: &str = ".approved-content-update";
const LAUNCHER_VERSION_FILE: &str = ".launcher-version";
const PENDING_LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.pending.json";
const PENDING_LAUNCHER_VERSION_FILE: &str = ".launcher-version.pending";
const GAME_MANIFEST_FILE: &str = ".game-manifest.json";
const GAME_VERSION_FILE: &str = ".game-version";
#[cfg(target_os = "macos")]
const MACOS_GAME_ARCHIVE_FILE_NAME: &str = "Rebellion2-Game-macOS.zip";
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
#[cfg(target_os = "macos")]
const STAGED_LAUNCHER_FILE_NAME: &str = ".rebellion2-launcher.next";
#[cfg(target_os = "linux")]
const STAGED_LAUNCHER_FILE_NAME: &str = ".rebellion2-launcher.next.AppImage";
#[cfg(target_os = "windows")]
const UPDATE_HELPER_FILE_NAME: &str = "rebellion2-update-helper.exe";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const UPDATE_HELPER_FILE_NAME: &str = "rebellion2-update-helper";

#[cfg(target_os = "windows")]
const LAUNCHER_FILE_NAME: &str = "rebellion2-launcher.exe";
#[cfg(target_os = "macos")]
const LAUNCHER_FILE_NAME: &str = "rebellion2-launcher";
#[cfg(target_os = "linux")]
const LAUNCHER_FILE_NAME: &str = "Rebellion2-launcher-Linux.AppImage";
/// Cached ownership session token, stored next to the launcher.
const SESSION_FILE: &str = ".session";

#[cfg(target_os = "windows")]
const GAME_EXE: &str = "Rebellion2.exe";
#[cfg(target_os = "linux")]
const GAME_EXE: &str = "Rebellion2.x86_64";
#[cfg(target_os = "linux")]
const UNITY_CRASH_HANDLER_EXE: &str = "UnityCrashHandler64";

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
/// A newer independent launcher release waiting for approval.
static PENDING_LAUNCHER_UPDATE: Mutex<Option<LauncherUpdate>> = Mutex::new(None);
/// A newer game-player release waiting for approval.
static PENDING_GAME_UPDATE: Mutex<Option<GameUpdate>> = Mutex::new(None);
/// Set after this launcher session downloads an update for its next start.
static LAUNCHER_UPDATE_STAGED: AtomicBool = AtomicBool::new(false);
/// A newer application release waiting for the user to approve its installation.
#[cfg(target_os = "windows")]
static PENDING_APPLICATION_UPDATE: Mutex<Option<ApplicationUpdate>> = Mutex::new(None);
/// The macOS application release waiting for the user to approve its installation.
#[cfg(target_os = "macos")]
static PENDING_APPLICATION_VERSION: Mutex<Option<String>> = Mutex::new(None);
/// Set when the user continues after an update failure so the content flow can proceed.
static APPLICATION_UPDATE_DISMISSED: AtomicBool = AtomicBool::new(false);
/// Set when the user keeps the currently installed game player for this session.
static GAME_UPDATE_DISMISSED: AtomicBool = AtomicBool::new(false);
/// Records whether the independent game channel answered coherently this scan.
static GAME_CHANNEL_AVAILABLE: AtomicBool = AtomicBool::new(false);
/// Set while an approved application update waits for renewed ownership verification.
static APPLICATION_UPDATE_AUTH_PENDING: AtomicBool = AtomicBool::new(false);
/// Set while an approved game update waits for renewed ownership verification.
static GAME_UPDATE_AUTH_PENDING: AtomicBool = AtomicBool::new(false);
/// Set while ownership verification returns the remote webview to bundled launcher content.
static GATE_RETURN_PENDING: AtomicBool = AtomicBool::new(false);
/// Set until the bundled launcher page is ready for its first channel scan.
static INITIAL_SCAN_PENDING: AtomicBool = AtomicBool::new(false);
/// Enables the local, network-free split-update walkthrough in debug builds.
#[cfg(debug_assertions)]
static PREVIEW_UPDATE_FLOW: AtomicBool = AtomicBool::new(false);

/// Ed25519 public key (hex) that must have signed executable update manifests.
const APPLICATION_UPDATE_PUBKEY: &str =
    "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d";

/// One platform's signed files in an independent launcher release.
#[derive(Debug, Clone, Deserialize)]
struct SignedLauncherLayer {
    manifest: String,
    #[serde(default = "default_launcher_blobs")]
    blobs: String,
    signature: String,
}

/// Platform artifacts published under one independent launcher version.
#[derive(Debug, Clone, Deserialize)]
struct LauncherPlatforms {
    #[cfg(any(target_os = "windows", test))]
    windows: Option<SignedLauncherLayer>,
    #[cfg(any(target_os = "macos", test))]
    macos: Option<SignedLauncherLayer>,
    #[cfg(any(target_os = "linux", test))]
    linux: Option<SignedLauncherLayer>,
}

/// The independent launcher release at `dist/launcher.json`.
#[derive(Debug, Clone, Deserialize)]
struct LauncherUpdate {
    version: String,
    platforms: LauncherPlatforms,
    #[serde(default, rename = "releaseNotes")]
    release_notes: Option<ReleaseNotesPointer>,
}

/// One platform's signed game-player files.
#[derive(Debug, Clone, Deserialize)]
struct SignedGameLayer {
    manifest: String,
    #[serde(default = "default_game_blobs")]
    blobs: String,
    signature: String,
}

/// Platform artifacts published under one game version.
#[derive(Debug, Clone, Deserialize)]
struct GamePlatforms {
    #[cfg(any(target_os = "windows", test))]
    windows: Option<SignedGameLayer>,
    #[cfg(any(target_os = "macos", test))]
    macos: Option<SignedGameLayer>,
    #[cfg(any(target_os = "linux", test))]
    linux: Option<SignedGameLayer>,
}

/// A game-player release paired atomically with protected content.
#[derive(Debug, Clone, Deserialize)]
struct GameUpdate {
    version: String,
    platforms: GamePlatforms,
    content: Latest,
}

fn default_launcher_blobs() -> String {
    "launcher-blobs/".to_string()
}

fn default_game_blobs() -> String {
    "game-blobs/".to_string()
}

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
    Update {
        base: String,
        latest: Latest,
        finishes_application_update: bool,
        release_notes: Option<ReleaseNotes>,
    },
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
#[derive(Debug, Clone, Deserialize)]
struct ReleaseNotes {
    version: String,
    sections: Vec<ReleaseNoteSection>,
    #[serde(default)]
    releases: Vec<VersionedReleaseNotes>,
}

/// A titled group of release-note items.
#[derive(Debug, Clone, Deserialize)]
struct ReleaseNoteSection {
    title: String,
    items: Vec<String>,
}

/// Release-note sections published for one historical version.
#[derive(Debug, Clone, Deserialize)]
struct VersionedReleaseNotes {
    version: String,
    sections: Vec<ReleaseNoteSection>,
}
fn default_blobs() -> String {
    "blobs/".to_string()
}

fn main() {
    #[cfg(debug_assertions)]
    PREVIEW_UPDATE_FLOW.store(
        std::env::args().any(|argument| argument == "--preview-update-flow"),
        Ordering::Relaxed,
    );
    #[cfg(debug_assertions)]
    let preview_update_flow = PREVIEW_UPDATE_FLOW.load(Ordering::Relaxed);
    #[cfg(not(debug_assertions))]
    let preview_update_flow = false;

    if !preview_update_flow && hand_off_pending_launcher_update() {
        return;
    }
    #[cfg(target_os = "macos")]
    if !preview_update_flow {
        if let Err(error) = recover_interrupted_macos_game_update() {
            log_line(&format!(
                "[launcher] couldn't recover an interrupted game update: {error}"
            ));
        }
    }

    // Seed the session token from cache if we have a non-expired one.
    if let Some(token) = read_cached_token() {
        *SESSION.lock().unwrap() = Some(token);
    }

    tauri::Builder::default()
        .setup(move |app| {
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
            let need_signin = !preview_update_flow && requires_ownership_gate(installed, repair);

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
                .resizable(false)
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
                    .resizable(false)
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
                if let Some(v) = requested_content_version() {
                    callback.push_str(&format!("&v={}", urlencoding::encode(&v)));
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

    #[cfg(debug_assertions)]
    if PREVIEW_UPDATE_FLOW.load(Ordering::Relaxed) {
        INITIAL_SCAN_PENDING.store(false, Ordering::Relaxed);
        show_preview_launcher_update(handle);
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
            Some(Pending::Update {
                base,
                latest,
                finishes_application_update,
                release_notes,
            }) => Some((
                base.clone(),
                latest.clone(),
                *finishes_application_update,
                release_notes.clone(),
            )),
            _ => None,
        }
    };
    // Remember the presigned zip for a possible first install without replacing an
    // update that was waiting for renewed authorization.
    if pending_update.is_none()
        && !APPLICATION_UPDATE_AUTH_PENDING.load(Ordering::Relaxed)
        && !GAME_UPDATE_AUTH_PENDING.load(Ordering::Relaxed)
    {
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
    if GAME_UPDATE_AUTH_PENDING.swap(false, Ordering::Relaxed) {
        start_game_update(handle);
        return;
    }
    if APPLICATION_UPDATE_AUTH_PENDING.swap(false, Ordering::Relaxed) {
        start_application_update(handle);
        return;
    }

    let pending_update = {
        let pending = PENDING.lock().unwrap();
        match pending.as_ref() {
            Some(Pending::Update {
                base,
                latest,
                finishes_application_update,
                release_notes,
            }) => Some((
                base.clone(),
                latest.clone(),
                *finishes_application_update,
                release_notes.clone(),
            )),
            _ => None,
        }
    };

    if let Some((base, latest, finishes_application_update, release_notes)) = pending_update {
        show_progress_ui_with_release_notes(handle, release_notes.as_ref());
        let handle = handle.clone();
        thread::spawn(move || run_update(&handle, &base, &latest, finishes_application_update));
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
    #[cfg(debug_assertions)]
    if PREVIEW_UPDATE_FLOW.load(Ordering::Relaxed) {
        on_preview_choice(handle, choice);
        return;
    }

    match choice {
        "play" => {
            APPLICATION_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            GAME_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            clear_approved_content_update();
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
        "launcher-update" => start_launcher_update(handle),
        "skip-launcher-update" => {
            *PENDING_LAUNCHER_UPDATE.lock().unwrap() = None;
            let handle = handle.clone();
            thread::spawn(move || scan_game_and_content(&handle));
        }
        "application-update" => start_application_update(handle),
        "game-update" => start_game_update(handle),
        "skip-game-update" => {
            GAME_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            GAME_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            *PENDING_GAME_UPDATE.lock().unwrap() = None;
            let handle = handle.clone();
            thread::spawn(move || scan_content_and_prompt(&handle));
        }
        "skip-application-update" => {
            APPLICATION_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            clear_approved_content_update();
            APPLICATION_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            let handle = handle.clone();
            thread::spawn(move || scan_content_and_prompt(&handle));
        }
        "retry-update-check" => {
            show_status_page(handle);
            let handle = handle.clone();
            thread::spawn(move || scan_and_prompt(&handle));
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
            if let Some(Pending::Update {
                base,
                latest,
                finishes_application_update,
                release_notes,
            }) = pending
            {
                show_progress_ui_with_release_notes(handle, release_notes.as_ref());
                let handle = handle.clone();
                thread::spawn(move || {
                    run_update(&handle, &base, &latest, finishes_application_update)
                });
            }
        }
        // Return from the gate's sign-in screens to the launcher's own screen.
        "back" => {
            APPLICATION_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            GAME_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
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

/// Handles the network-free debug walkthrough used to inspect the split update flow.
#[cfg(debug_assertions)]
fn on_preview_choice(handle: &tauri::AppHandle, choice: &str) {
    match choice {
        "launcher-update" => {
            show_progress_ui_with_release_notes(handle, Some(&preview_launcher_release_notes()));
            update_progress(handle, 100, "Launcher update downloaded.");
            let handle = handle.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(700));
                write_screen(
                    &handle,
                    &game_update_screen("0.0.26", Some(&preview_game_release_notes()), true),
                );
            });
        }
        "skip-launcher-update" => write_screen(
            handle,
            &game_update_screen("0.0.26", Some(&preview_game_release_notes()), false),
        ),
        "game-update" => {
            show_progress_ui_with_release_notes(handle, Some(&preview_game_release_notes()));
            update_progress(handle, 100, "Game update complete.");
            let handle = handle.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(700));
                write_screen(&handle, &launcher_staged_ready_screen("0.0.26"));
            });
        }
        "skip-game-update" => write_screen(handle, &up_to_date_screen("0.0.25")),
        "play" | "quit" => handle.exit(0),
        _ => {}
    }
}

/// Shows the first screen in the network-free debug update walkthrough.
#[cfg(debug_assertions)]
fn show_preview_launcher_update(handle: &tauri::AppHandle) {
    write_screen(
        handle,
        &launcher_update_screen("1.0.1", Some(&preview_launcher_release_notes())),
    );
}

/// Supplies launcher-only notes to the network-free debug walkthrough.
#[cfg(debug_assertions)]
fn preview_launcher_release_notes() -> ReleaseNotes {
    ReleaseNotes {
        version: "1.0.1".to_string(),
        sections: vec![ReleaseNoteSection {
            title: "Fixes".to_string(),
            items: vec!["Kept the launcher open after downloading an update.".to_string()],
        }],
        releases: Vec::new(),
    }
}

/// Supplies game-only notes to the network-free debug walkthrough.
#[cfg(debug_assertions)]
fn preview_game_release_notes() -> ReleaseNotes {
    ReleaseNotes {
        version: "0.0.26".to_string(),
        sections: vec![ReleaseNoteSection {
            title: "Fixes".to_string(),
            items: vec!["Fixed an example gameplay issue.".to_string()],
        }],
        releases: Vec::new(),
    }
}

// -- application updates -----------------------------------------------------

/// Returns a newer independent launcher release without consulting game content.
fn check_launcher_update() -> Option<String> {
    let base = content_base()?;
    let current = current_launcher_version()?;
    let update: LauncherUpdate = fetch_json(&format!("{base}dist/launcher.json"), None).ok()?;
    launcher_layer(&update)?;
    if version_gt(&update.version, &current) {
        let version = update.version.clone();
        *PENDING_LAUNCHER_UPDATE.lock().unwrap() = Some(update);
        Some(version)
    } else {
        None
    }
}

/// Selects the signed launcher layer for the current operating system.
fn launcher_layer(update: &LauncherUpdate) -> Option<&SignedLauncherLayer> {
    #[cfg(target_os = "windows")]
    {
        update.platforms.windows.as_ref()
    }
    #[cfg(target_os = "macos")]
    {
        update.platforms.macos.as_ref()
    }
    #[cfg(target_os = "linux")]
    {
        update.platforms.linux.as_ref()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = update;
        None
    }
}

/// Starts an approved launcher-only update without changing game or content state.
fn start_launcher_update(handle: &tauri::AppHandle) {
    let Some(update) = PENDING_LAUNCHER_UPDATE.lock().unwrap().clone() else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    let release_notes = content_base().and_then(|base| {
        fetch_release_notes_pointer(
            &base,
            &update.version,
            update.release_notes.as_ref(),
            current_launcher_version().as_deref(),
        )
    });
    show_progress_ui_with_release_notes(handle, release_notes.as_ref());
    let handle = handle.clone();
    thread::spawn(move || match do_launcher_update(&handle, &update) {
        Ok(()) => {
            log_line(&format!(
                "[launcher] launcher update {} staged.",
                update.version
            ));
            LAUNCHER_UPDATE_STAGED.store(true, Ordering::Relaxed);
            *PENDING_LAUNCHER_UPDATE.lock().unwrap() = None;
            update_progress(&handle, 100, "Launcher update downloaded.");
            thread::sleep(Duration::from_millis(900));
            scan_game_and_content(&handle);
        }
        Err(error) => {
            log_line(&format!("[launcher] launcher update failed: {error}"));
            show_message(
                &handle,
                "Update failed",
                "The launcher update failed — see launcher.log.",
                Some(("Continue", "skip-launcher-update")),
            );
        }
    });
}

/// Downloads and stages only the launcher executable and its handoff helper.
fn do_launcher_update(
    handle: &tauri::AppHandle,
    update: &LauncherUpdate,
) -> Result<(), Box<dyn std::error::Error>> {
    let layer = launcher_layer(update).ok_or("launcher release does not support this platform")?;
    let base = content_base().ok_or("launcher update channel is not configured")?;
    let manifest_bytes = fetch_bytes(&format!("{base}{}", layer.manifest))?;
    verify_signed_manifest(&manifest_bytes, &layer.signature)?;
    let manifest = Manifest::from_json(&manifest_bytes)?;
    let (launcher_entry, helper_entry) = validate_launcher_manifest(&manifest, &update.version)?;
    let blobs = PublicHttpBlobs {
        base: format!("{base}{}", layer.blobs),
    };
    update_progress(handle, 10, "Downloading launcher update…");
    let launcher = fetch_verified_blob(&blobs, launcher_entry)?;
    update_progress(handle, 60, "Downloading launcher helper…");
    let helper = fetch_verified_blob(&blobs, helper_entry)?;
    let metadata_dir = install_dir()?;
    write_executable(&metadata_dir.join(UPDATE_HELPER_FILE_NAME), &helper)?;
    write_executable(&metadata_dir.join(STAGED_LAUNCHER_FILE_NAME), &launcher)?;
    fs::write(
        metadata_dir.join(PENDING_LAUNCHER_MANIFEST_FILE),
        manifest_bytes,
    )?;
    fs::write(
        metadata_dir.join(PENDING_LAUNCHER_VERSION_FILE),
        &update.version,
    )?;
    Ok(())
}

/// Fetches one content-addressed executable and verifies its declared hash.
fn fetch_verified_blob(
    blobs: &PublicHttpBlobs,
    entry: &FileEntry,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bytes = blobs.fetch(&entry.sha256)?;
    if sha256_hex(&bytes) != entry.sha256 {
        return Err(format!("{} does not match its signed hash", entry.path).into());
    }
    Ok(bytes)
}

/// Writes a launcher component and makes it executable on Unix platforms.
fn write_executable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    ensure_executable(path)
}

/// Ensures a program can be executed on Unix without changing Windows metadata.
fn ensure_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Validates that a launcher manifest contains exactly the launcher and helper.
fn validate_launcher_manifest<'a>(
    manifest: &'a Manifest,
    version: &str,
) -> io::Result<(&'a FileEntry, &'a FileEntry)> {
    if manifest.version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher manifest version does not match its channel pointer",
        ));
    }
    if manifest.files.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launcher manifest must contain exactly two files",
        ));
    }
    let launcher = manifest
        .files
        .iter()
        .find(|entry| entry.path == LAUNCHER_FILE_NAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "launcher is missing"))?;
    let helper = manifest
        .files
        .iter()
        .find(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "launcher helper is missing"))?;
    Ok((launcher, helper))
}

/// Returns the independently installed launcher version with bootstrap fallbacks.
fn current_launcher_version() -> Option<String> {
    install_dir()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(LAUNCHER_VERSION_FILE)).ok())
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .or_else(|| {
            LAUNCHER_VERSION
                .filter(|version| !version.is_empty())
                .map(str::to_string)
        })
        .or_else(current_application_version)
}

/// Returns whether a downloaded launcher is waiting for handoff.
fn launcher_update_is_staged() -> bool {
    install_dir()
        .map(|directory| directory.join(STAGED_LAUNCHER_FILE_NAME).is_file())
        .unwrap_or(false)
}

/// Returns whether this open launcher downloaded an update for its next start.
fn launcher_update_was_staged() -> bool {
    LAUNCHER_UPDATE_STAGED.load(Ordering::Relaxed) && launcher_update_is_staged()
}

/// Returns a newer game-player release paired with content of the same version.
fn check_game_update() -> Option<String> {
    GAME_CHANNEL_AVAILABLE.store(false, Ordering::Relaxed);
    let base = content_base()?;
    let current = current_game_version()?;
    let update: GameUpdate = fetch_json(&format!("{base}dist/game.json"), None).ok()?;
    if update.content.version != update.version || game_layer(&update).is_none() {
        log_line("[launcher] refusing an incoherent game release.");
        return None;
    }
    GAME_CHANNEL_AVAILABLE.store(true, Ordering::Relaxed);
    if GAME_UPDATE_DISMISSED.load(Ordering::Relaxed) {
        return None;
    }
    if version_gt(&update.version, &current) {
        let version = update.version.clone();
        *PENDING_GAME_UPDATE.lock().unwrap() = Some(update);
        Some(version)
    } else {
        None
    }
}

/// Selects the signed game-player layer for the current operating system.
fn game_layer(update: &GameUpdate) -> Option<&SignedGameLayer> {
    #[cfg(target_os = "windows")]
    {
        update.platforms.windows.as_ref()
    }
    #[cfg(target_os = "macos")]
    {
        update.platforms.macos.as_ref()
    }
    #[cfg(target_os = "linux")]
    {
        update.platforms.linux.as_ref()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = update;
        None
    }
}

/// Starts a game-player update and its separately protected matching content update.
fn start_game_update(handle: &tauri::AppHandle) {
    let Some(update) = PENDING_GAME_UPDATE.lock().unwrap().clone() else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    if SESSION.lock().unwrap().is_none() {
        GAME_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
        let notes = fetch_release_notes(&content_base().unwrap_or_default(), &update.content);
        show_update_signin(handle, notes.as_ref());
        return;
    }
    let notes = fetch_release_notes(&content_base().unwrap_or_default(), &update.content);
    show_progress_ui_with_release_notes(handle, notes.as_ref());
    let handle = handle.clone();
    thread::spawn(move || run_game_update(&handle, &update));
}

/// Applies a signed game-player layer before updating its matching protected content.
fn run_game_update(handle: &tauri::AppHandle, update: &GameUpdate) {
    match do_game_update(handle, update) {
        Ok(changed) => {
            log_line(&format!(
                "[launcher] game update to {} complete ({changed} files).",
                update.version
            ));
            let Some(base) = content_base() else {
                show_message(
                    handle,
                    "Update failed",
                    "The game content channel is not configured.",
                    None,
                );
                return;
            };
            let release_notes = fetch_release_notes(&base, &update.content);
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: update.content.clone(),
                finishes_application_update: false,
                release_notes,
            });
            run_update(handle, &base, &update.content, false);
        }
        Err(error) => {
            log_line(&format!("[launcher] game update failed: {error}"));
            show_message(
                handle,
                "Update failed",
                "The game update failed — see launcher.log.",
                Some(("Continue", "skip-game-update")),
            );
        }
    }
}

/// Downloads and applies only files owned by the game player.
fn do_game_update(
    handle: &tauri::AppHandle,
    update: &GameUpdate,
) -> Result<usize, Box<dyn std::error::Error>> {
    let layer = game_layer(update).ok_or("game release does not support this platform")?;
    let base = content_base().ok_or("game update channel is not configured")?;
    let manifest_bytes = fetch_bytes(&format!("{base}{}", layer.manifest))?;
    verify_signed_manifest(&manifest_bytes, &layer.signature)?;
    let remote = Manifest::from_json(&manifest_bytes)?;
    validate_game_manifest(&remote, &update.version)?;
    let metadata_dir = install_dir()?;
    let blobs = PublicHttpBlobs {
        base: format!("{base}{}", layer.blobs),
    };

    #[cfg(target_os = "macos")]
    let changed = {
        let archive = validate_macos_game_archive_manifest(&remote)?;
        update_progress(
            handle,
            5,
            &format!("Updating game… {}", human_bytes(archive.size)),
        );
        let bytes = fetch_verified_blob(&blobs, archive)?;
        install_macos_game_archive(&bytes)?;
        1
    };

    #[cfg(not(target_os = "macos"))]
    let changed = {
        let game_dir = install_dir()?;
        let local = read_game_manifest(&metadata_dir)
            .unwrap_or(snapshot_application_files(&game_dir, &remote)?);
        let plan = diff(Some(&local), &remote);
        update_progress(
            handle,
            5,
            &format!("Updating game… {}", human_bytes(plan.download_size())),
        );
        let changed = plan.changed.len();
        apply(&plan, &game_dir, &blobs)?;
        #[cfg(target_os = "linux")]
        {
            ensure_executable(&game_dir.join(GAME_EXE))?;
            let crash_handler = game_dir.join(UNITY_CRASH_HANDLER_EXE);
            if crash_handler.is_file() {
                ensure_executable(&crash_handler)?;
            }
        }
        changed
    };

    fs::write(metadata_dir.join(GAME_MANIFEST_FILE), manifest_bytes)?;
    fs::write(metadata_dir.join(GAME_VERSION_FILE), &update.version)?;
    Ok(changed)
}

/// Validates the single signed archive used for macOS game replacement.
#[cfg(target_os = "macos")]
fn validate_macos_game_archive_manifest(manifest: &Manifest) -> io::Result<&FileEntry> {
    if manifest.files.len() != 1 || manifest.files[0].path != MACOS_GAME_ARCHIVE_FILE_NAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS game manifest must contain exactly one game archive",
        ));
    }
    Ok(&manifest.files[0])
}

/// Replaces the nested macOS game bundle while keeping a rollback copy.
#[cfg(target_os = "macos")]
fn install_macos_game_archive(bytes: &[u8]) -> io::Result<()> {
    let metadata_dir = install_dir()?;
    let archive_path = metadata_dir.join(".game-update.zip");
    fs::write(&archive_path, bytes)?;

    let game = game_path()?;
    let resources = game
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid game bundle path"))?;
    let staging_root = resources.join(".game-update-staging");
    let staged_game = staging_root.join(MACOS_GAME_APP_NAME);
    let backup_game = resources.join(".game-update-backup");
    remove_directory_if_present(&staging_root)?;
    remove_directory_if_present(&backup_game)?;
    fs::create_dir_all(&staging_root)?;

    let extract_status = std::process::Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&archive_path)
        .arg(&staging_root)
        .status()?;
    if !extract_status.success() || !staged_game.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "could not extract the macOS game update",
        ));
    }
    sign_macos_bundle(&staged_game)?;

    let had_installed_game = game.is_dir();
    if had_installed_game {
        fs::rename(&game, &backup_game)?;
    }
    if let Err(error) = fs::rename(&staged_game, &game) {
        if had_installed_game {
            let _ = fs::rename(&backup_game, &game);
        }
        return Err(error);
    }

    let outer_bundle = macos_bundle_contents_dir(&std::env::current_exe()?)?
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid app bundle"))?
        .to_path_buf();
    if let Err(error) = sign_macos_bundle(&outer_bundle) {
        let _ = fs::remove_dir_all(&game);
        if had_installed_game {
            let _ = fs::rename(&backup_game, &game);
            let _ = sign_macos_bundle(&outer_bundle);
        }
        return Err(error);
    }

    remove_directory_if_present(&backup_game)?;
    remove_directory_if_present(&staging_root)?;
    let _ = fs::remove_file(archive_path);
    Ok(())
}

/// Restores the last complete macOS game after an interrupted directory swap.
#[cfg(target_os = "macos")]
fn recover_interrupted_macos_game_update() -> io::Result<()> {
    let game = game_path()?;
    let resources = game
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid game bundle path"))?;
    let backup_game = resources.join(".game-update-backup");
    if backup_game.is_dir() && !game.exists() {
        fs::rename(&backup_game, &game)?;
    } else if backup_game.is_dir() {
        fs::remove_dir_all(backup_game)?;
    }
    remove_directory_if_present(&resources.join(".game-update-staging"))
}

/// Removes a known temporary directory when it exists.
#[cfg(target_os = "macos")]
fn remove_directory_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Applies an ad-hoc signature to a macOS bundle after local assembly.
#[cfg(target_os = "macos")]
fn sign_macos_bundle(bundle: &Path) -> io::Result<()> {
    let status = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(bundle)
        .status()?;
    if !status.success() {
        return Err(io::Error::other("could not sign the macOS bundle"));
    }
    Ok(())
}

/// Validates that a game manifest cannot overwrite launcher or content files.
fn validate_game_manifest(manifest: &Manifest, version: &str) -> io::Result<()> {
    if manifest.version != version || manifest.files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "game manifest version or file list is invalid",
        ));
    }
    for entry in &manifest.files {
        let path = Path::new(&entry.path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || entry.path.eq_ignore_ascii_case(LAUNCHER_FILE_NAME)
            || entry.path.eq_ignore_ascii_case(UPDATE_HELPER_FILE_NAME)
            || is_launcher_metadata_path(path)
            || is_unmanaged_application_path(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "game manifest contains a launcher, content, or unsafe path",
            ));
        }
    }
    Ok(())
}

/// Returns whether a game layer is attempting to overwrite launcher state.
fn is_launcher_metadata_path(path: &Path) -> bool {
    let Some(Component::Normal(component)) = path.components().next() else {
        return false;
    };
    matches!(
        component.to_string_lossy().to_ascii_lowercase().as_str(),
        ".launcher-version"
            | ".launcher-manifest.json"
            | ".launcher-version.pending"
            | ".launcher-manifest.pending.json"
            | ".rebellion2-launcher.next.exe"
            | ".rebellion2-launcher.next"
            | ".rebellion2-launcher.next.appimage"
            | ".session"
            | "launcher.log"
    )
}

/// Reads the installed game-player manifest independently from launcher metadata.
#[cfg(not(target_os = "macos"))]
fn read_game_manifest(metadata_dir: &Path) -> Option<Manifest> {
    fs::read(metadata_dir.join(GAME_MANIFEST_FILE))
        .ok()
        .and_then(|bytes| Manifest::from_json(&bytes).ok())
        .or_else(|| {
            bundled_metadata_dir()
                .and_then(|directory| fs::read(directory.join(GAME_MANIFEST_FILE)).ok())
                .and_then(|bytes| Manifest::from_json(&bytes).ok())
        })
}

/// Returns the installed game-player version independently from launcher metadata.
fn current_game_version() -> Option<String> {
    install_dir()
        .ok()
        .and_then(|directory| fs::read_to_string(directory.join(GAME_VERSION_FILE)).ok())
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
        .or_else(|| {
            bundled_metadata_dir()
                .and_then(|directory| fs::read_to_string(directory.join(GAME_VERSION_FILE)).ok())
                .map(|version| version.trim().to_string())
                .filter(|version| !version.is_empty())
        })
        .or_else(current_application_version)
}

/// Returns installer metadata embedded in a macOS application bundle.
fn bundled_metadata_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_bundle_contents_dir(&std::env::current_exe().ok()?)
            .ok()
            .map(|contents| contents.join("Resources").join("InstallerMetadata"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

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
        Ok(Some(update)) => {
            let version = update.version;
            *PENDING_APPLICATION_VERSION.lock().unwrap() = Some(version.clone());
            Some(version)
        }
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
        if !begin_application_update_or_request_authorization(handle, &update.version) {
            return;
        }
        let release_notes = fetch_current_release_notes(&update.version);
        show_progress_ui_with_release_notes(handle, release_notes.as_ref());
        let handle = handle.clone();
        thread::spawn(move || run_windows_application_update(&handle, &update));
    }
}

#[cfg(target_os = "macos")]
fn start_application_update(handle: &tauri::AppHandle) {
    let version = PENDING_APPLICATION_VERSION.lock().unwrap().clone();
    let Some(version) = version else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    if !begin_application_update_or_request_authorization(handle, &version) {
        return;
    }
    let release_notes = fetch_current_release_notes(&version);
    show_progress_ui_with_release_notes(handle, release_notes.as_ref());
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        run_macos_application_update(&handle).await;
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn start_application_update(_handle: &tauri::AppHandle) {}

/// Returns whether protected content can be downloaded, or opens ownership verification.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn begin_application_update_or_request_authorization(
    handle: &tauri::AppHandle,
    version: &str,
) -> bool {
    if SESSION.lock().unwrap().is_some() {
        return true;
    }

    APPLICATION_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
    let release_notes = fetch_current_release_notes(version);
    show_update_signin(handle, release_notes.as_ref());
    false
}

#[cfg(target_os = "macos")]
async fn run_macos_application_update(handle: &tauri::AppHandle) {
    update_progress(handle, 0, "Preparing update\u{2026}");
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

    let (base, latest) = match prepare_application_content(&update.version, None) {
        Ok(release) => release,
        Err(error) => {
            show_application_update_preparation_error(handle, &update.version, error.as_ref());
            return;
        }
    };

    if let Err(error) = store_approved_content_update(&update.version) {
        log_line(&format!(
            "[launcher] couldn't remember the approved content update: {error}"
        ));
        show_message(
            handle,
            "Update failed",
            "The application update could not be prepared \u{2014} see launcher.log.",
            Some(("Continue", "skip-application-update")),
        );
        return;
    }

    let mut downloaded = 0_u64;
    let progress_handle = handle.clone();
    let finish_handle = handle.clone();
    let result = update
        .download_and_install(
            move |chunk_size, content_length| {
                downloaded = downloaded.saturating_add(chunk_size as u64);
                let percent = content_length
                    .filter(|total| *total > 0)
                    .map(|total| 5 + downloaded.saturating_mul(40) / total)
                    .unwrap_or(5)
                    .min(45);
                update_progress(
                    &progress_handle,
                    percent,
                    &format!("Updating application\u{2026} {}", human_bytes(downloaded)),
                );
            },
            move || update_progress(&finish_handle, 48, "Installing application\u{2026}"),
        )
        .await;

    match result {
        Ok(()) => {
            log_line(&format!(
                "[launcher] macOS application update to {} installed.",
                update.version
            ));
            let release_notes = fetch_release_notes(&base, &latest);
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: latest.clone(),
                finishes_application_update: true,
                release_notes,
            });
            run_update(handle, &base, &latest, true);
        }
        Err(error) => {
            clear_approved_content_update();
            show_macos_application_update_error(handle, &error);
        }
    }
}

/// Resolves and authorizes the protected content paired with an application release.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn prepare_application_content(
    version: &str,
    embedded: Option<Latest>,
) -> Result<(String, Latest), Box<dyn std::error::Error>> {
    let base = content_base().ok_or("application update channel is not configured")?;
    let latest = match embedded {
        Some(latest) => latest,
        None => content_release_for_application(fetch_latest(&base)?, Some(version)),
    };
    if latest.version != version {
        return Err("application and content releases do not match".into());
    }

    let token = SESSION.lock().unwrap().clone();
    let _: Manifest = fetch_json(&format!("{base}{}", latest.manifest), token.as_deref())?;
    Ok((base, latest))
}

/// Reports a failure that occurs before any application files are changed.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn show_application_update_preparation_error(
    handle: &tauri::AppHandle,
    version: &str,
    error: &(dyn std::error::Error + 'static),
) {
    if is_authorization_required(error) {
        log_line("[launcher] update authorization expired before the application download.");
        clear_token();
        APPLICATION_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
        let release_notes = fetch_current_release_notes(version);
        show_update_signin(handle, release_notes.as_ref());
        return;
    }

    log_line(&format!(
        "[launcher] application update could not be prepared: {error}"
    ));
    show_message(
        handle,
        "Update failed",
        "The update could not be prepared — see launcher.log.",
        Some(("Continue", "skip-application-update")),
    );
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

/// One validated semantic version split into precedence-bearing identifiers.
struct SemanticVersion<'a> {
    core: [&'a str; 3],
    prerelease: Vec<&'a str>,
}

/// True if semantic version `a` is newer than `b`.
fn version_gt(a: &str, b: &str) -> bool {
    compare_versions(a, b) == VersionOrdering::Greater
}

/// Compares semantic versions without truncating arbitrarily large numeric identifiers.
fn compare_versions(a: &str, b: &str) -> VersionOrdering {
    let (Some(a), Some(b)) = (parse_semantic_version(a), parse_semantic_version(b)) else {
        return VersionOrdering::Equal;
    };
    for (left, right) in a.core.iter().zip(b.core.iter()) {
        let comparison = compare_numeric_identifier(left, right);
        if comparison != VersionOrdering::Equal {
            return comparison;
        }
    }

    match (a.prerelease.is_empty(), b.prerelease.is_empty()) {
        (true, true) => VersionOrdering::Equal,
        (true, false) => VersionOrdering::Greater,
        (false, true) => VersionOrdering::Less,
        (false, false) => {
            for index in 0..a.prerelease.len().max(b.prerelease.len()) {
                let comparison = match (a.prerelease.get(index), b.prerelease.get(index)) {
                    (None, Some(_)) => VersionOrdering::Less,
                    (Some(_), None) => VersionOrdering::Greater,
                    (Some(left), Some(right)) => compare_prerelease_identifier(left, right),
                    (None, None) => VersionOrdering::Equal,
                };
                if comparison != VersionOrdering::Equal {
                    return comparison;
                }
            }
            VersionOrdering::Equal
        }
    }
}

/// Parses the semantic-version subset accepted by the release workflow.
fn parse_semantic_version(version: &str) -> Option<SemanticVersion<'_>> {
    let mut build_parts = version.split('+');
    let version = build_parts.next()?;
    let build = build_parts.next();
    if build_parts.next().is_some() || build.is_some_and(|value| !valid_identifiers(value, false)) {
        return None;
    }

    let (core, prerelease) = version
        .split_once('-')
        .map_or((version, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    let core = core.split('.').collect::<Vec<_>>();
    if core.len() != 3
        || core
            .iter()
            .any(|identifier| !valid_numeric_identifier(identifier))
        || prerelease.is_some_and(|value| !valid_identifiers(value, true))
    {
        return None;
    }

    Some(SemanticVersion {
        core: [core[0], core[1], core[2]],
        prerelease: prerelease.map_or_else(Vec::new, |value| value.split('.').collect()),
    })
}

/// Returns whether a dot-separated identifier sequence is valid SemVer text.
fn valid_identifiers(identifiers: &str, enforce_numeric_zeroes: bool) -> bool {
    identifiers.split('.').all(|identifier| {
        !identifier.is_empty()
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!enforce_numeric_zeroes
                || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                || valid_numeric_identifier(identifier))
    })
}

/// Returns whether a numeric identifier is nonempty and has no leading zero.
fn valid_numeric_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.bytes().all(|byte| byte.is_ascii_digit())
        && (identifier == "0" || !identifier.starts_with('0'))
}

/// Compares decimal integers by magnitude without converting them to a fixed-width type.
fn compare_numeric_identifier(left: &str, right: &str) -> VersionOrdering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

/// Compares two prerelease identifiers using semantic-version precedence rules.
fn compare_prerelease_identifier(left: &str, right: &str) -> VersionOrdering {
    let left_numeric = left.bytes().all(|byte| byte.is_ascii_digit());
    let right_numeric = right.bytes().all(|byte| byte.is_ascii_digit());
    match (left_numeric, right_numeric) {
        (true, true) => compare_numeric_identifier(left, right),
        (true, false) => VersionOrdering::Less,
        (false, true) => VersionOrdering::Greater,
        (false, false) => left.cmp(right),
    }
}

/// Installs one application/content release and leaves the launcher ready to play.
#[cfg(target_os = "windows")]
fn run_windows_application_update(handle: &tauri::AppHandle, update: &ApplicationUpdate) {
    update_progress(handle, 0, "Preparing update…");
    let (base, latest) = match prepare_application_content(&update.version, update.content.clone())
    {
        Ok(release) => release,
        Err(error) => {
            show_application_update_preparation_error(handle, &update.version, error.as_ref());
            return;
        }
    };

    match do_application_update(handle, update) {
        Ok(changed) => {
            log_line(&format!(
                "[launcher] application update to {} staged ({changed} files).",
                update.version
            ));
            let release_notes = fetch_release_notes(&base, &latest);
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: latest.clone(),
                finishes_application_update: true,
                release_notes,
            });
            run_update(handle, &base, &latest, true);
        }
        Err(error) => {
            clear_approved_content_update();
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
    verify_signed_manifest(&manifest_bytes, &update.signature)?;
    let remote = Manifest::from_json(&manifest_bytes)?;
    validate_application_manifest(&remote, &update.version)?;

    let install_dir = install_dir()?;
    write_approved_content_update(&install_dir, &update.version)?;
    let local = read_application_manifest(&install_dir)
        .unwrap_or(snapshot_application_files(&install_dir, &remote)?);
    let plan = diff(Some(&local), &remote);
    update_progress(
        handle,
        5,
        &format!(
            "Updating application… {}",
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
    update_progress(
        handle,
        48,
        "Application files ready. Updating game content…",
    );
    Ok(changed + usize::from(launcher_changed))
}

/// Verifies that an executable-update manifest was signed by the release pipeline.
fn verify_signed_manifest(
    manifest_bytes: &[u8],
    signature: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let key: [u8; 32] = hex::decode(APPLICATION_UPDATE_PUBKEY)?
        .as_slice()
        .try_into()
        .map_err(|_| "bad executable-update public key length")?;
    let verifying = VerifyingKey::from_bytes(&key)?;
    let signature = Signature::from_slice(&hex::decode(signature)?)?;
    verifying
        .verify(manifest_bytes, &signature)
        .map_err(|_| "executable-update manifest signature does not match".into())
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
#[cfg(not(target_os = "macos"))]
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
struct PublicHttpBlobs {
    base: String,
}

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

/// Starts the installed helper outside the launcher process so it can promote the
/// staged executable after this process exits.
fn start_update_helper(relaunch: bool) -> io::Result<()> {
    let metadata_dir = install_dir()?;
    let helper = metadata_dir.join(UPDATE_HELPER_FILE_NAME);
    let launcher = launcher_path()?;
    let staged = metadata_dir.join(STAGED_LAUNCHER_FILE_NAME);
    let mut command = std::process::Command::new(helper);
    command
        .arg("--launcher")
        .arg(&launcher)
        .arg("--staged")
        .arg(&staged)
        .arg("--metadata-dir")
        .arg(&metadata_dir)
        .arg("--wait-pid")
        .arg(std::process::id().to_string());
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

/// Returns the replaceable launcher artifact for the current platform.
#[cfg(not(target_os = "linux"))]
fn launcher_path() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// Returns the mounted AppImage's outer file, falling back to the process for development builds.
#[cfg(target_os = "linux")]
fn launcher_path() -> io::Result<PathBuf> {
    std::env::var_os("APPIMAGE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)
}

/// Completes a previously interrupted launcher handoff before any window appears.
fn hand_off_pending_launcher_update() -> bool {
    let Ok(metadata_dir) = install_dir() else {
        return false;
    };
    if !metadata_dir.join(STAGED_LAUNCHER_FILE_NAME).is_file() {
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

/// Reads the channel pointer and decides what to show.
fn scan_and_prompt(handle: &tauri::AppHandle) {
    update_status(handle, "Checking for updates…");

    if let Some(version) = check_launcher_update() {
        log_line(&format!("[launcher] launcher update available: {version}"));
        show_launcher_update(handle, &version);
        return;
    }

    scan_game_and_content(handle);
}

/// Checks the game/player channel before scanning protected content.
fn scan_game_and_content(handle: &tauri::AppHandle) {
    if let Some(version) = check_game_update() {
        log_line(&format!("[launcher] game update available: {version}"));
        show_game_update(handle, &version);
        return;
    }

    // The combined application channel is only a bridge for installations that
    // cannot yet read the independent game channel.
    if !GAME_CHANNEL_AVAILABLE.load(Ordering::Relaxed) {
        if let Some(version) = check_application_update(handle) {
            log_line(&format!(
                "[launcher] application update available: {version}"
            ));
            show_application_update(handle, &version);
            return;
        }
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
    let approved_version = read_approved_content_update_version();

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
            let game_version = current_game_version();
            let latest = content_release_for_application(published, game_version.as_deref());
            let continue_approved = should_continue_approved_update(
                approved_version.as_deref(),
                &latest.version,
                installed_version.as_deref(),
            );
            if approved_version.is_some() && !continue_approved {
                clear_approved_content_update();
            }
            // Fail closed before offering either first install or update.
            if !content_matches_application(game_version.as_deref(), &latest.version) {
                clear_approved_content_update();
                let installed_matches_game = content_matches_application(
                    game_version.as_deref(),
                    installed_version.as_deref().unwrap_or_default(),
                );
                log_line(&format!(
                    "[launcher] refusing content {} for game {}.",
                    latest.version,
                    game_version.as_deref().unwrap_or("unknown")
                ));
                if installed_present && installed_matches_game {
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
                clear_approved_content_update();
                show_ready_to_install(handle, Some(&latest.version));
            } else if !installed_present {
                clear_approved_content_update();
                log_line("[launcher] first install requires ownership verification.");
                show_message(
                    handle,
                    "Sign in required",
                    "Verify ownership before downloading the game.",
                    Some(("Sign in", "signin")),
                );
            } else if installed_version.as_deref() == Some(latest.version.as_str()) {
                clear_approved_content_update();
                log_line(&format!("[launcher] up to date ({}).", latest.version));
                show_up_to_date(handle, &latest.version);
            } else {
                log_line(&format!(
                    "[launcher] update available: {} -> {}",
                    installed_version.as_deref().unwrap_or("unknown"),
                    latest.version
                ));
                if continue_approved {
                    continue_approved_update(handle, &base, &latest);
                } else {
                    prompt_update(handle, &base, &latest, &content_dir);
                }
            }
        }
        Err(err) => {
            log_line(&format!("[launcher] update check failed ({err})."));
            show_update_check_failed(
                handle,
                installed_present.then_some(installed_version.as_deref().unwrap_or("unknown")),
                &err.to_string(),
            );
        }
    }
}

fn continue_approved_update(handle: &tauri::AppHandle, base: &str, latest: &Latest) {
    log_line(&format!(
        "[launcher] continuing approved release {} after application restart.",
        latest.version
    ));
    let release_notes = fetch_release_notes(base, latest);
    *PENDING.lock().unwrap() = Some(Pending::Update {
        base: base.to_string(),
        latest: latest.clone(),
        finishes_application_update: false,
        release_notes: release_notes.clone(),
    });

    if SESSION.lock().unwrap().is_none() {
        show_update_signin(handle, release_notes.as_ref());
        return;
    }

    show_progress_ui_with_release_notes(handle, release_notes.as_ref());
    let handle = handle.clone();
    let base = base.to_string();
    let latest = latest.clone();
    thread::spawn(move || run_update(&handle, &base, &latest, false));
}

/// Requests authorization when necessary, then computes the update size and prompts.
fn prompt_update(handle: &tauri::AppHandle, base: &str, latest: &Latest, content_dir: &Path) {
    let token = SESSION.lock().unwrap().clone();
    let release_notes = fetch_release_notes(base, latest);
    *PENDING.lock().unwrap() = Some(Pending::Update {
        base: base.to_string(),
        latest: latest.clone(),
        finishes_application_update: false,
        release_notes: release_notes.clone(),
    });

    let Some(token) = token else {
        show_update_signin(handle, release_notes.as_ref());
        return;
    };
    let remote: Option<Manifest> =
        match fetch_json(&format!("{base}{}", latest.manifest), Some(&token)) {
            Ok(remote) => Some(remote),
            Err(err) if is_authorization_required(err.as_ref()) => {
                clear_token();
                show_update_signin(handle, release_notes.as_ref());
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
    let installed_version = install_dir()
        .ok()
        .and_then(|directory| read_installed_version(&directory.join("Content")));
    fetch_release_notes_pointer(
        base,
        &latest.version,
        latest.release_notes.as_ref(),
        installed_version.as_deref(),
    )
}

/// Downloads and validates release notes from either the launcher or game channel.
fn fetch_release_notes_pointer(
    base: &str,
    version: &str,
    pointer: Option<&ReleaseNotesPointer>,
    installed_version: Option<&str>,
) -> Option<ReleaseNotes> {
    let pointer = pointer?;
    let bytes = fetch_channel_bytes(&format!("{base}{}", pointer.path), None).ok()?;
    let notes = parse_release_notes(&bytes, version, &pointer.sha256)?;
    Some(aggregate_release_notes(notes, installed_version))
}

/// Parses release notes only when their version and digest match the release pointer.
fn parse_release_notes(bytes: &[u8], version: &str, sha256: &str) -> Option<ReleaseNotes> {
    if sha256_hex(bytes) != sha256 {
        return None;
    }

    let notes = serde_json::from_slice::<ReleaseNotes>(bytes).ok()?;
    if notes.version != version
        || !release_note_sections_are_valid(&notes.sections)
        || notes.releases.iter().any(|release| {
            release.version.trim().is_empty()
                || version_gt(&release.version, &notes.version)
                || !release_note_sections_are_valid(&release.sections)
        })
    {
        return None;
    }

    Some(notes)
}

/// Returns whether every release-note section has a title and at least one item.
fn release_note_sections_are_valid(sections: &[ReleaseNoteSection]) -> bool {
    !sections.is_empty()
        && sections
            .iter()
            .all(|section| !section.title.trim().is_empty() && !section.items.is_empty())
}

/// Merges every published release newer than the installed content into one view.
fn aggregate_release_notes(notes: ReleaseNotes, installed_version: Option<&str>) -> ReleaseNotes {
    let Some(installed_version) = installed_version else {
        return notes;
    };
    if notes.releases.is_empty() {
        return notes;
    }

    let mut selected = notes
        .releases
        .iter()
        .filter(|release| {
            version_gt(&release.version, installed_version)
                && !version_gt(&release.version, &notes.version)
        })
        .collect::<Vec<_>>();
    selected.sort_by(|left, right| compare_versions(&left.version, &right.version));
    if selected.is_empty() {
        return notes;
    }

    let mut section_titles = Vec::new();
    for expected_title in ["Additions", "Changes", "Fixes"] {
        if selected.iter().any(|release| {
            release
                .sections
                .iter()
                .any(|section| section.title == expected_title)
        }) {
            section_titles.push(expected_title.to_string());
        }
    }
    for section in &notes.sections {
        if !section_titles.contains(&section.title) {
            section_titles.push(section.title.clone());
        }
    }
    for release in &selected {
        for release_section in &release.sections {
            if !section_titles.contains(&release_section.title) {
                section_titles.push(release_section.title.clone());
            }
        }
    }
    let mut sections = section_titles
        .into_iter()
        .map(|title| ReleaseNoteSection {
            title,
            items: Vec::new(),
        })
        .collect::<Vec<_>>();
    for release in selected {
        for release_section in &release.sections {
            let section = sections
                .iter_mut()
                .find(|section| section.title == release_section.title)
                .expect("release-note section was initialized");
            for item in &release_section.items {
                if !section.items.contains(item) {
                    section.items.push(item.clone());
                }
            }
        }
    }
    sections.retain(|section| !section.items.is_empty());

    ReleaseNotes {
        version: notes.version,
        sections,
        releases: Vec::new(),
    }
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

fn run_update(
    handle: &tauri::AppHandle,
    base: &str,
    latest: &Latest,
    finishes_application_update: bool,
) {
    match do_update(handle, base, latest, finishes_application_update) {
        Ok(fetched) => {
            log_line(&format!(
                "[launcher] update to {} complete ({fetched} files).",
                latest.version
            ));
            if finishes_application_update {
                finish_application_update(handle, &latest.version);
            } else {
                clear_approved_content_update();
                update_progress(handle, 100, "Update complete.");
                // Let the filled bar sit a beat, then land on the up-to-date screen.
                thread::sleep(Duration::from_millis(900));
                show_up_to_date(handle, &latest.version);
            }
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
            if !finishes_application_update {
                clear_approved_content_update();
            }
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
            let release_notes = fetch_release_notes(base, latest);
            if finishes_application_update {
                show_required_update_signin(handle, release_notes.as_ref());
            } else {
                show_update_signin(handle, release_notes.as_ref());
            }
        }
        Err(err) => {
            if !finishes_application_update {
                clear_approved_content_update();
            }
            log_line(&format!("[launcher] update failed: {err}"));
            update_progress(handle, 0, "Update failed — see launcher.log");
        }
    }
}

/// Leaves a completed application update ready to play in the current launcher.
/// A staged Windows launcher is promoted when the user next starts the launcher.
fn finish_application_update(handle: &tauri::AppHandle, version: &str) {
    clear_approved_content_update();
    update_progress(handle, 100, "Update complete.");
    thread::sleep(Duration::from_millis(900));
    show_up_to_date(handle, version);
}

fn do_update(
    handle: &tauri::AppHandle,
    base: &str,
    latest: &Latest,
    finishes_application_update: bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content_dir = install_dir()?.join("Content");
    let token = SESSION.lock().unwrap().clone();
    let checking_label = if finishes_application_update {
        "Updating game content…"
    } else {
        "Checking what changed…"
    };
    let progress_start = if finishes_application_update { 50 } else { 5 };
    update_progress(handle, progress_start, checking_label);

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
    let download_label = if finishes_application_update {
        format!(
            "Updating game content: {changed} files, {}",
            human_bytes(bytes)
        )
    } else {
        format!("Downloading {changed} files, {}", human_bytes(bytes))
    };
    update_progress(handle, 5, &download_label);

    let blobs = HttpBlobs {
        base: format!("{base}{}", latest.blobs),
        token,
        handle: handle.clone(),
        total: bytes,
        done: AtomicU64::new(0),
        progress_start,
        progress_end: 95,
        application_update: finishes_application_update,
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
    progress_start: u64,
    progress_end: u64,
    application_update: bool,
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
                .saturating_mul(self.progress_end.saturating_sub(self.progress_start))
                .checked_div(self.total)
                .map(|percent| (self.progress_start + percent).min(self.progress_end))
                .unwrap_or(self.progress_start);
            if percent != last_percent {
                last_percent = percent;
                let label = if self.application_update {
                    format!(
                        "Updating game content… {} / {}",
                        human_bytes(done),
                        human_bytes(self.total)
                    )
                } else {
                    format!(
                        "Downloading update… {} / {}",
                        human_bytes(done),
                        human_bytes(self.total)
                    )
                };
                update_progress(&self.handle, percent, &label);
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
            if let (Some(base), Some(version)) = (content_base(), requested_content_version()) {
                let token = SESSION.lock().unwrap().clone();
                if let Ok(manifest) = fetch_json::<Manifest>(
                    &format!("{base}dist/manifest-{version}.json"),
                    token.as_deref(),
                ) {
                    let _ = store_manifest_and_version(&content_dir, &manifest, &version);
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

fn read_approved_content_update_version() -> Option<String> {
    let directory = install_dir().ok()?;
    read_approved_content_update(&directory)
}

fn read_approved_content_update(directory: &Path) -> Option<String> {
    let version = fs::read_to_string(directory.join(APPROVED_CONTENT_UPDATE_FILE))
        .ok()?
        .trim()
        .to_string();
    (!version.is_empty()).then_some(version)
}

#[cfg(target_os = "macos")]
fn store_approved_content_update(version: &str) -> io::Result<()> {
    let directory = install_dir()?;
    write_approved_content_update(&directory, version)
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn write_approved_content_update(directory: &Path, version: &str) -> io::Result<()> {
    let version = version.trim();
    if version.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "approved content update version is empty",
        ));
    }
    fs::write(directory.join(APPROVED_CONTENT_UPDATE_FILE), version)
}

fn clear_approved_content_update() {
    let Ok(directory) = install_dir() else {
        return;
    };
    if let Err(error) = fs::remove_file(directory.join(APPROVED_CONTENT_UPDATE_FILE)) {
        if error.kind() != io::ErrorKind::NotFound {
            log_line(&format!(
                "[launcher] couldn't clear the approved content update: {error}"
            ));
        }
    }
}

fn should_continue_approved_update(
    approved_version: Option<&str>,
    release_version: &str,
    installed_version: Option<&str>,
) -> bool {
    approved_version == Some(release_version) && installed_version != Some(release_version)
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
    if let Ok(release) = fetch_json::<GameUpdate>(&format!("{base}dist/game.json"), None) {
        return Ok(release.content);
    }

    // Compatibility path for launchers installed before game and launcher
    // releases were separated.
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
    match requested_content_version() {
        Some(version) => format!("{auth_base}?v={}", urlencoding::encode(&version)),
        _ => auth_base,
    }
}

/// Returns the game version whose protected content this installation requires.
fn requested_content_version() -> Option<String> {
    current_game_version().or_else(|| {
        CONTENT_VERSION
            .filter(|version| !version.is_empty())
            .map(str::to_string)
    })
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

/// One shared full-window layout: kicker + REBELLION II wordmark top-aligned, an
/// optional spinner, a status line, a progress bar, and buttons pinned toward the
/// bottom.
fn render(kicker: &str, spinner: bool, status: &str, buttons: &str) -> String {
    render_with_content(kicker, spinner, status, "", buttons)
}

/// Renders the shared launcher layout with optional content above its actions.
fn render_with_content(
    kicker: &str,
    spinner: bool,
    status: &str,
    content: &str,
    buttons: &str,
) -> String {
    render_card(kicker, spinner, status, "sub", content, buttons)
}

fn render_card(
    kicker: &str,
    spinner: bool,
    status: &str,
    status_class: &str,
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
        r##"<!doctype html><html><head><meta charset="utf-8"><style>*{{box-sizing:border-box}}html,body{{height:100%;margin:0}}body{{font-family:"Segoe UI",system-ui,sans-serif;color:#e8ecf6;background:radial-gradient(1200px 800px at 70% -10%,#1a2547 0%,transparent 55%),radial-gradient(900px 700px at 10% 110%,#241238 0%,transparent 50%),linear-gradient(180deg,#0b1226,#05070f);display:flex;overflow:hidden;user-select:none}}body::before{{content:"";position:fixed;inset:0;background-image:radial-gradient(1.5px 1.5px at 20% 30%,#fff 50%,transparent),radial-gradient(1px 1px at 80% 20%,#cdd 50%,transparent),radial-gradient(1.5px 1.5px at 60% 70%,#fff 50%,transparent),radial-gradient(1px 1px at 35% 80%,#bcd 50%,transparent),radial-gradient(1px 1px at 90% 60%,#fff 50%,transparent),radial-gradient(1.5px 1.5px at 12% 65%,#eef 50%,transparent);opacity:.5;pointer-events:none}}.card{{position:relative;width:100%;height:100%;overflow:hidden;padding:40px 34px 30px;display:flex;flex-direction:column;text-align:center}}.kicker{{letter-spacing:.42em;font-size:11px;color:#ffcf4d;text-transform:uppercase;margin:0 0 12px;opacity:.9}}h1{{margin:0;font-size:clamp(24px,8vw,40px);font-weight:800;letter-spacing:.1em;line-height:1.05}}h1 .two{{color:#ffcf4d}}.sub{{margin:16px auto 18px;max-width:34ch;color:#8a93ad;font-size:14.5px;line-height:1.6;min-height:20px}}.has-content .sub{{margin-bottom:0}}.failure-error{{margin:10px auto 0;max-width:34ch;color:#ff6b73;font-size:14.5px;font-weight:700;line-height:1.6}}.spin{{width:28px;height:28px;border:2.5px solid rgba(255,255,255,.14);border-top-color:#ffcf4d;border-radius:50%;animation:sp .8s linear infinite;margin:4px auto}}@keyframes sp{{to{{transform:rotate(360deg)}}}}.bar{{width:100%;height:7px;background:rgba(255,255,255,.08);border-radius:5px;overflow:hidden;display:none;margin-top:4px}}.f{{height:100%;width:0%;background:linear-gradient(90deg,#ffcf4d,#ffb43d);transition:width .3s}}.p{{font-size:11.5px;color:#8a93ad;min-height:0;margin-top:8px}}.has-content .p:empty{{margin:0}}.release-changes{{min-height:0;overflow-y:auto;padding-right:8px;text-align:left;scrollbar-color:#59637b rgba(255,255,255,.06);scrollbar-width:thin}}.release-changes::-webkit-scrollbar{{width:7px}}.release-changes::-webkit-scrollbar-track{{background:rgba(255,255,255,.06);border-radius:4px}}.release-changes::-webkit-scrollbar-thumb{{background:#59637b;border-radius:4px}}.patch-title{{margin:0 0 9px;color:#e8ecf6;font-size:13px}}.save-warning{{margin:0 0 16px;color:#ff3434;font-size:12px;font-weight:900;line-height:1.25;text-align:center;text-transform:uppercase;letter-spacing:.025em}}.changes+.changes{{margin-top:9px}}.changes h2{{margin:0 0 3px;color:#b9c2da;font-size:9px;text-transform:uppercase}}.changes ul{{margin:0;padding-left:18px;color:#cbd2e3;font-size:11px;line-height:1.45}}.changes li+li{{margin-top:5px}}.btns{{flex:none;margin-top:auto}}.b{{display:flex;align-items:center;justify-content:center;width:100%;padding:14px 18px;margin:11px 0 0;border-radius:11px;font-size:15px;font-weight:700;text-decoration:none;transition:transform .08s ease,filter .15s ease}}.b.primary{{background:#ffcf4d;color:#0a0e1a;box-shadow:0 8px 22px rgba(255,207,77,.22)}}.b.primary:hover{{filter:brightness(1.06);transform:translateY(-1px)}}.b.secondary{{background:rgba(255,255,255,.06);color:#c9d1e6;border:1px solid rgba(255,255,255,.14)}}.b.secondary:hover{{filter:brightness(1.18);transform:translateY(-1px)}}.b.disabled{{background:rgba(255,255,255,.06);color:#5b6479;cursor:default}}</style></head><body><main class="{card_class}"><p class="kicker">{kicker}</p><h1>REBELLION <span class="two">II</span></h1><p class="{status_class}" id="s">{status}</p>{spin}<div class="bar" id="bar"><div class="f" id="f"></div></div><div class="p" id="p"></div>{content}<div class="btns">{buttons}</div></main><script>window.rebSetProgress=function(p,l){{var b=document.getElementById("bar");if(b)b.style.display="block";var f=document.getElementById("f");if(f)f.style.width=p+"%";var pe=document.getElementById("p");if(pe)pe.textContent=p>0?p+"%":"";if(l){{var s=document.getElementById("s");if(s)s.textContent=l;}}}};window.rebSetStatus=function(l){{var s=document.getElementById("s");if(s)s.textContent=l;}};</script></body></html>"##,
        kicker = kicker,
        status = status,
        spin = spin,
        content = content,
        card_class = card_class,
        status_class = status_class,
        buttons = buttons,
    )
}

fn button(label: &str, choice: &str) -> String {
    format!(
        "<a class=\"b primary\" href=\"{}?choice={}\">{}</a>",
        ACT, choice, label
    )
}

fn secondary_button(label: &str, choice: &str) -> String {
    format!(
        "<a class=\"b secondary\" href=\"{}?choice={}\">{}</a>",
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

fn show_up_to_date(handle: &tauri::AppHandle, version: &str) {
    let screen = if launcher_update_was_staged() {
        launcher_staged_ready_screen(version)
    } else {
        up_to_date_screen(version)
    };
    write_screen(handle, &screen);
}

fn up_to_date_screen(version: &str) -> String {
    render(
        "Up to date",
        false,
        &format!("Version {version} is up to date."),
        &button("Launch game", "play"),
    )
}

/// Builds the ready screen after staging a launcher update and checking the game.
fn launcher_staged_ready_screen(version: &str) -> String {
    render(
        "Ready to play",
        false,
        &format!(
            "Game version {version} is up to date. The launcher update will apply next time you start the launcher."
        ),
        &button("Play game", "play"),
    )
}

fn show_update_check_failed(
    handle: &tauri::AppHandle,
    installed_version: Option<&str>,
    error: &str,
) {
    write_screen(
        handle,
        &update_check_failed_screen(installed_version, error),
    );
}

fn update_check_failed_screen(installed_version: Option<&str>, error: &str) -> String {
    let note = installed_version.map_or_else(
        || "Check your connection, then retry.".to_string(),
        |version| {
            format!("Version {version} is still available to play. You can retry or launch it now.")
        },
    );
    let reason = update_check_failure_reason(error);
    let content = format!(
        "<p class=\"failure-error\">Error: {}</p>",
        html_escape(reason)
    );
    let mut buttons = button("Retry", "retry-update-check");
    if installed_version.is_some() {
        buttons.push_str(&secondary_button("Launch game", "play"));
    }
    render_with_content("Launcher", false, &note, &content, &buttons)
}

fn update_check_failure_reason(error: &str) -> &str {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("timed out") || normalized.contains("timeout") {
        "The update server did not respond in time."
    } else if normalized.contains("connection failed")
        || normalized.contains("connect error")
        || normalized.contains("connection refused")
        || normalized.contains("dns")
    {
        "The launcher could not reach the update server."
    } else if normalized.contains("no longer available") {
        "Update information for this launcher version is no longer available."
    } else {
        "The update server returned invalid update information."
    }
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

/// Offers an independent launcher update without presenting game release notes.
fn show_launcher_update(handle: &tauri::AppHandle, version: &str) {
    let release_notes = content_base().and_then(|base| {
        PENDING_LAUNCHER_UPDATE
            .lock()
            .unwrap()
            .clone()
            .and_then(|update| {
                fetch_release_notes_pointer(
                    &base,
                    &update.version,
                    update.release_notes.as_ref(),
                    current_launcher_version().as_deref(),
                )
            })
    });
    write_screen(
        handle,
        &launcher_update_screen(version, release_notes.as_ref()),
    );
}

/// Builds the confirmation screen for an independent launcher update.
fn launcher_update_screen(version: &str, notes: Option<&ReleaseNotes>) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=launcher-update\">Update launcher</a>\
         <a class=\"b secondary\" href=\"{a}?choice=skip-launcher-update\">Not now</a>",
        a = ACT,
    );
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Launcher update available",
        false,
        &format!("Launcher version {version} is available. This does not update the game."),
        &release_notes,
        &buttons,
    )
}

/// Offers a game-player update without conflating it with a launcher release.
fn show_game_update(handle: &tauri::AppHandle, version: &str) {
    let release_notes = content_base()
        .zip(PENDING_GAME_UPDATE.lock().unwrap().clone())
        .and_then(|(base, update)| fetch_release_notes(&base, &update.content));
    write_screen(
        handle,
        &game_update_screen(
            version,
            release_notes.as_ref(),
            launcher_update_was_staged(),
        ),
    );
}

/// Builds the confirmation screen for a game-player and content release.
fn game_update_screen(
    version: &str,
    notes: Option<&ReleaseNotes>,
    launcher_update_staged: bool,
) -> String {
    let buttons = if launcher_update_staged {
        format!(
            "<a class=\"b primary\" href=\"{a}?choice=game-update\">Update game</a>\
             <a class=\"b secondary\" href=\"{a}?choice=play\">Play game</a>",
            a = ACT,
        )
    } else {
        format!(
            "<a class=\"b primary\" href=\"{a}?choice=game-update\">Update game</a>\
             <a class=\"b secondary\" href=\"{a}?choice=skip-game-update\">Not now</a>",
            a = ACT,
        )
    };
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    let status = if launcher_update_staged {
        format!(
            "Game version {version} is available. The launcher update is downloaded and will apply next time you start the launcher."
        )
    } else {
        format!("Game version {version} is available.")
    };
    render_with_content(
        "Game update available",
        false,
        &status,
        &release_notes,
        &buttons,
    )
}

/// Offers an available application update without beginning its download.
fn show_application_update(handle: &tauri::AppHandle, version: &str) {
    let release_notes = fetch_current_release_notes(version);
    write_screen(
        handle,
        &application_update_screen(version, release_notes.as_ref()),
    );
}

/// Builds the confirmation screen shown before an application update downloads.
fn application_update_screen(version: &str, notes: Option<&ReleaseNotes>) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=application-update\">Update</a>\
         <a class=\"b secondary\" href=\"{a}?choice=skip-application-update\">Not Now</a>",
        a = ACT,
    );
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Update available",
        false,
        &format!(
            "Version {version} is available. You can launch the game when the update is complete."
        ),
        &release_notes,
        &buttons,
    )
}

/// Fetches notes for the currently published release when it matches an update prompt.
fn fetch_current_release_notes(version: &str) -> Option<ReleaseNotes> {
    let base = content_base()?;
    let latest = fetch_latest(&base).ok()?;
    if latest.version != version {
        return None;
    }
    fetch_release_notes(&base, &latest)
}

/// Shows the ownership prompt without hiding the update's release notes.
fn show_update_signin(handle: &tauri::AppHandle, notes: Option<&ReleaseNotes>) {
    write_screen(handle, &update_signin_screen(notes));
}

/// Builds the ownership prompt shown before protected update content downloads.
fn update_signin_screen(notes: Option<&ReleaseNotes>) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=signin\">Sign in &amp; update</a>\
         <a class=\"b secondary\" href=\"{a}?choice=play\">Launch game</a>",
        a = ACT,
    );
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Sign in required",
        false,
        "Verify ownership to download this update.",
        &release_notes,
        &buttons,
    )
}

/// Shows ownership verification when a partially installed application update must finish.
fn show_required_update_signin(handle: &tauri::AppHandle, notes: Option<&ReleaseNotes>) {
    write_screen(handle, &required_update_signin_screen(notes));
}

/// Builds the ownership prompt used after application files have been staged.
fn required_update_signin_screen(notes: Option<&ReleaseNotes>) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=signin\">Sign in &amp; Continue</a>\
         <a class=\"b secondary\" href=\"{a}?choice=quit\">Close launcher</a>",
        a = ACT,
    );
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Sign in required",
        false,
        "Verify ownership to finish this update.",
        &release_notes,
        &buttons,
    )
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
    write_screen(handle, &progress_screen(None));
    update_progress(handle, 0, "Preparing update\u{2026}");
}

/// Shows update progress while retaining cumulative release notes.
fn show_progress_ui_with_release_notes(handle: &tauri::AppHandle, notes: Option<&ReleaseNotes>) {
    write_screen(handle, &progress_screen(notes));
    update_progress(handle, 0, "Preparing update\u{2026}");
}

/// Builds the update progress screen with optional cumulative release notes.
fn progress_screen(notes: Option<&ReleaseNotes>) -> String {
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Updating",
        false,
        "Preparing update\u{2026}",
        &release_notes,
        "",
    )
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

#[cfg(target_os = "windows")]
fn install_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "launcher has no parent directory"))
}

#[cfg(target_os = "linux")]
fn install_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not configured"))?;
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    let directory = linux_data_dir(&home, xdg_data_home.as_deref());
    fs::create_dir_all(&directory)?;
    Ok(directory)
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

#[cfg(any(target_os = "linux", test))]
fn linux_data_dir(home: &Path, xdg_data_home: Option<&Path>) -> PathBuf {
    xdg_data_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local").join("share"))
        .join("rebellion2")
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
    if let Some(version) = requested_content_version() {
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

    fn signed_launcher_layer(manifest: &str) -> SignedLauncherLayer {
        SignedLauncherLayer {
            manifest: manifest.to_string(),
            blobs: default_launcher_blobs(),
            signature: "signature".to_string(),
        }
    }

    fn signed_game_layer(manifest: &str) -> SignedGameLayer {
        SignedGameLayer {
            manifest: manifest.to_string(),
            blobs: default_game_blobs(),
            signature: "signature".to_string(),
        }
    }

    #[test]
    fn launcher_layer_with_all_platforms_selects_current_platform() {
        let update = LauncherUpdate {
            version: "1.2.3".to_string(),
            platforms: LauncherPlatforms {
                windows: Some(signed_launcher_layer("windows")),
                macos: Some(signed_launcher_layer("macos")),
                linux: Some(signed_launcher_layer("linux")),
            },
            release_notes: None,
        };

        let layer = launcher_layer(&update).unwrap();

        #[cfg(target_os = "windows")]
        assert_eq!(layer.manifest, "windows");
        #[cfg(target_os = "macos")]
        assert_eq!(layer.manifest, "macos");
        #[cfg(target_os = "linux")]
        assert_eq!(layer.manifest, "linux");
    }

    #[test]
    fn game_layer_with_all_platforms_selects_current_platform() {
        let update = GameUpdate {
            version: "1.2.3".to_string(),
            platforms: GamePlatforms {
                windows: Some(signed_game_layer("windows")),
                macos: Some(signed_game_layer("macos")),
                linux: Some(signed_game_layer("linux")),
            },
            content: Latest {
                version: "1.2.3".to_string(),
                manifest: "content".to_string(),
                blobs: "blobs".to_string(),
                release_notes: None,
            },
        };

        let layer = game_layer(&update).unwrap();

        #[cfg(target_os = "windows")]
        assert_eq!(layer.manifest, "windows");
        #[cfg(target_os = "macos")]
        assert_eq!(layer.manifest, "macos");
        #[cfg(target_os = "linux")]
        assert_eq!(layer.manifest, "linux");
    }

    #[test]
    fn game_layer_without_current_platform_returns_none() {
        let update = GameUpdate {
            version: "1.2.3".to_string(),
            platforms: GamePlatforms {
                windows: None,
                macos: None,
                linux: None,
            },
            content: Latest {
                version: "1.2.3".to_string(),
                manifest: "content".to_string(),
                blobs: "blobs".to_string(),
                release_notes: None,
            },
        };

        assert!(game_layer(&update).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn game_executable_on_linux_matches_unity_player_name() {
        assert_eq!(GAME_EXE, "Rebellion2.x86_64");
        assert_eq!(UNITY_CRASH_HANDLER_EXE, "UnityCrashHandler64");
    }

    #[cfg(unix)]
    #[test]
    fn write_executable_on_unix_sets_execute_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("program");

        write_executable(&path, b"program").unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }

    #[test]
    fn launcher_update_screen_with_release_notes_identifies_launcher_update() {
        let notes = release_notes();

        let screen = launcher_update_screen("1.2.3", Some(&notes));

        assert!(screen.contains("Launcher update available"));
        assert!(screen.contains("Launcher version 1.2.3 is available."));
        assert!(screen.contains("This does not update the game."));
        assert!(screen.contains("choice=launcher-update\">Update launcher"));
        assert!(screen.contains("Fixed update handling."));
    }

    #[test]
    fn game_update_screen_after_launcher_download_keeps_play_available() {
        let screen = game_update_screen("0.0.26", Some(&release_notes()), true);

        assert!(screen.contains("Game update available"));
        assert!(screen.contains("choice=game-update\">Update game"));
        assert!(screen.contains("choice=play\">Play game"));
        assert!(screen.contains("will apply next time you start the launcher"));
        assert!(screen.contains("Fixed update handling."));
    }

    #[test]
    fn launcher_staged_ready_screen_keeps_play_available() {
        let screen = launcher_staged_ready_screen("0.0.25");

        assert!(screen.contains("Ready to play"));
        assert!(screen.contains("Game version 0.0.25 is up to date."));
        assert!(screen.contains("will apply next time you start the launcher"));
        assert!(screen.contains("choice=play\">Play game"));
    }

    #[test]
    fn launcher_manifest_with_only_launcher_and_helper_returns_entries() {
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);

        let (launcher, helper) = validate_launcher_manifest(&manifest, "1.2.3").unwrap();

        assert_eq!(launcher.path, LAUNCHER_FILE_NAME);
        assert_eq!(helper.path, UPDATE_HELPER_FILE_NAME);
    }

    #[test]
    fn launcher_manifest_with_game_file_returns_error() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                LAUNCHER_FILE_NAME,
                UPDATE_HELPER_FILE_NAME,
                "Rebellion2.exe",
            ],
        );

        assert!(validate_launcher_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn game_manifest_with_player_files_returns_ok() {
        let manifest = application_manifest("0.0.26", &["Rebellion2.exe", "Data/shared.dat"]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_ok());
    }

    #[test]
    fn game_manifest_with_launcher_file_returns_error() {
        let manifest = application_manifest("0.0.26", &[LAUNCHER_FILE_NAME]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_err());
    }

    #[test]
    fn game_manifest_with_content_file_returns_error() {
        let manifest = application_manifest("0.0.26", &["Content/catalog.xml"]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_err());
    }

    #[test]
    fn game_manifest_with_launcher_metadata_returns_error() {
        let manifest = application_manifest("0.0.26", &[".launcher-version"]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_err());
    }

    #[test]
    fn game_manifest_with_staged_linux_launcher_returns_error() {
        let manifest = application_manifest("0.0.26", &[".rebellion2-launcher.next.AppImage"]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_game_archive_manifest_with_one_archive_returns_entry() {
        let manifest = application_manifest("0.0.26", &[MACOS_GAME_ARCHIVE_FILE_NAME]);

        let entry = validate_macos_game_archive_manifest(&manifest).unwrap();

        assert_eq!(entry.path, MACOS_GAME_ARCHIVE_FILE_NAME);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_game_archive_manifest_with_loose_files_returns_error() {
        let manifest = application_manifest("0.0.26", &["Rebellion2.app/Contents/Info.plist"]);

        assert!(validate_macos_game_archive_manifest(&manifest).is_err());
    }

    #[test]
    fn update_channels_deserialize_independent_launcher_and_game_versions() {
        let launcher: LauncherUpdate = serde_json::from_str(
            r#"{
                "version":"1.2.3",
                "platforms":{
                    "windows":{"manifest":"dist/launcher-manifest-windows-1.2.3.json","signature":"signed"},
                    "macos":{"manifest":"dist/launcher-manifest-macos-1.2.3.json","signature":"signed"},
                    "linux":{"manifest":"dist/launcher-manifest-linux-1.2.3.json","signature":"signed"}
                }
            }"#,
        )
        .unwrap();
        let game: GameUpdate = serde_json::from_str(
            r#"{
                "version":"0.0.26",
                "platforms":{
                    "windows":{"manifest":"dist/game-manifest-windows-0.0.26.json","signature":"signed"},
                    "macos":{"manifest":"dist/game-manifest-macos-0.0.26.json","signature":"signed"},
                    "linux":{"manifest":"dist/game-manifest-linux-0.0.26.json","signature":"signed"}
                },
                "content":{"version":"0.0.26","manifest":"dist/manifest-0.0.26.json"}
            }"#,
        )
        .unwrap();

        assert_eq!(launcher.version, "1.2.3");
        assert!(launcher.platforms.windows.is_some());
        assert!(launcher.platforms.macos.is_some());
        assert!(launcher.platforms.linux.is_some());
        assert_eq!(game.version, "0.0.26");
        assert!(game.platforms.windows.is_some());
        assert!(game.platforms.macos.is_some());
        assert!(game.platforms.linux.is_some());
        assert_eq!(game.content.version, "0.0.26");
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
    fn application_update_screen_with_available_update_offers_update_and_skip() {
        let screen = application_update_screen("1.2.3", None);

        assert!(screen.contains("Version 1.2.3 is available."));
        assert!(screen.contains("You can launch the game when the update is complete."));
        assert!(!screen.contains("launcher will close"));
        assert!(!screen.contains("restart"));
        assert!(screen.contains("choice=application-update\">Update"));
        assert!(screen.contains("choice=skip-application-update\">Not Now"));
    }

    #[test]
    fn up_to_date_screen_with_confirmed_version_displays_version() {
        let screen = up_to_date_screen("1.2.3");

        assert!(screen.contains("Version 1.2.3 is up to date."));
        assert!(screen.contains("choice=play\">Launch game"));
    }

    #[test]
    fn update_check_failed_screen_with_installed_game_offers_retry_and_launch() {
        let screen = update_check_failed_screen(Some("1.2.3"), "Connection Failed");

        assert!(screen.contains(">Launcher</p>"));
        assert!(screen.contains("Version 1.2.3 is still available to play."));
        assert!(screen.contains("failure-error"));
        assert!(screen.contains("Error: The launcher could not reach the update server."));
        assert!(screen.find("Version 1.2.3").unwrap() < screen.find("Error:").unwrap());
        assert!(screen.find("Error:").unwrap() < screen.find(">Retry</a>").unwrap());
        assert!(screen.contains("choice=retry-update-check\">Retry"));
        assert!(screen.contains("choice=play\">Launch game"));
    }

    #[test]
    fn update_check_failed_screen_without_installed_game_offers_only_retry() {
        let screen = update_check_failed_screen(None, "operation timed out");

        assert!(screen.contains(">Launcher</p>"));
        assert!(screen.contains("Check your connection, then retry."));
        assert!(screen.contains("did not respond in time"));
        assert!(screen.contains("choice=retry-update-check\">Retry"));
        assert!(!screen.contains("choice=play"));
    }

    #[test]
    fn update_check_failure_reason_with_invalid_response_explains_response_failure() {
        assert_eq!(
            update_check_failure_reason("expected value at line 1"),
            "The update server returned invalid update information."
        );
    }

    #[test]
    fn approved_content_update_round_trips_its_version() {
        let directory = tempfile::tempdir().unwrap();

        write_approved_content_update(directory.path(), " 1.2.3 ").unwrap();

        assert_eq!(
            read_approved_content_update(directory.path()).as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn approved_content_update_rejects_an_empty_version() {
        let directory = tempfile::tempdir().unwrap();

        assert!(write_approved_content_update(directory.path(), "  ").is_err());
        assert!(read_approved_content_update(directory.path()).is_none());
    }

    #[test]
    fn approved_update_continues_for_the_matching_incomplete_release() {
        assert!(should_continue_approved_update(
            Some("1.2.3"),
            "1.2.3",
            Some("1.2.2")
        ));
    }

    #[test]
    fn approved_update_does_not_continue_for_a_stale_release() {
        assert!(!should_continue_approved_update(
            Some("1.2.2"),
            "1.2.3",
            Some("1.2.2")
        ));
    }

    #[test]
    fn approved_update_does_not_continue_when_content_is_current() {
        assert!(!should_continue_approved_update(
            Some("1.2.3"),
            "1.2.3",
            Some("1.2.3")
        ));
    }

    #[test]
    fn compare_versions_with_prerelease_and_stable_orders_stable_last() {
        assert_eq!(
            compare_versions("1.0.0-beta.2", "1.0.0-beta.1"),
            VersionOrdering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0-beta", "1.0.0"),
            VersionOrdering::Less
        );
    }

    #[test]
    fn compare_versions_with_large_identifiers_preserves_numeric_precision() {
        assert_eq!(
            compare_versions("9007199254740993.0.0", "9007199254740992.0.0"),
            VersionOrdering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0-beta.9007199254740993", "1.0.0-beta.9007199254740992"),
            VersionOrdering::Greater
        );
    }

    #[test]
    fn compare_versions_with_build_metadata_ignores_metadata() {
        assert_eq!(
            compare_versions("1.0.0+build.2", "1.0.0+build.1"),
            VersionOrdering::Equal
        );
    }

    #[test]
    fn parse_semantic_version_with_invalid_leading_zero_returns_none() {
        assert!(parse_semantic_version("01.0.0").is_none());
        assert!(parse_semantic_version("1.0.0-beta.01").is_none());
    }

    #[test]
    fn application_update_screen_with_release_notes_displays_release_content() {
        let notes = release_notes();

        let screen = application_update_screen("1.2.3", Some(&notes));

        assert!(screen.contains("Patch Notes"));
        assert!(screen.contains("Fixes"));
        assert!(screen.contains("Fixed update handling."));
    }

    #[test]
    fn update_signin_screen_with_release_notes_displays_release_content() {
        let notes = release_notes();

        let screen = update_signin_screen(Some(&notes));

        assert!(screen.contains("Verify ownership to download this update."));
        assert!(screen.contains("Patch Notes"));
        assert!(screen.contains("Fixed update handling."));
        assert!(screen.contains("choice=signin\">Sign in &amp; update"));
    }

    #[test]
    fn required_update_signin_screen_with_staged_application_prevents_launching_game() {
        let screen = required_update_signin_screen(None);

        assert!(screen.contains("Verify ownership to finish this update."));
        assert!(screen.contains("choice=signin\">Sign in &amp; Continue"));
        assert!(screen.contains("choice=quit\">Close launcher"));
        assert!(!screen.contains("choice=play"));
    }

    #[test]
    fn update_available_screen_with_release_notes_displays_release_content() {
        let notes = release_notes();
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
    fn parse_release_notes_with_legacy_document_defaults_to_empty_history() {
        let bytes = br#"{"version":"1.2.3","sections":[{"title":"Fixes","items":["Fixed update handling."]}]}"#;

        let notes = parse_release_notes(bytes, "1.2.3", &sha256_hex(bytes))
            .expect("legacy release notes should remain readable");

        assert!(notes.releases.is_empty());
        assert_eq!(notes.sections[0].items, vec!["Fixed update handling."]);
    }

    #[test]
    fn aggregate_release_notes_with_lagging_install_merges_intermediate_releases() {
        let notes = cumulative_release_notes();

        let aggregated = aggregate_release_notes(notes, Some("0.0.15"));

        assert_eq!(aggregated.version, "0.0.21");
        assert_eq!(aggregated.sections.len(), 3);
        assert_eq!(aggregated.sections[0].title, "Additions");
        assert_eq!(
            aggregated.sections[0].items,
            vec!["Added 0.0.16 feature.", "Added 0.0.21 feature."]
        );
        assert_eq!(aggregated.sections[1].title, "Changes");
        assert_eq!(
            aggregated.sections[1].items,
            vec!["Changed 0.0.17 behavior."]
        );
        assert_eq!(aggregated.sections[2].title, "Fixes");
        assert_eq!(
            aggregated.sections[2].items,
            vec!["Fixed 0.0.16 issue.", "Fixed 0.0.21 issue."]
        );
        assert!(aggregated.releases.is_empty());
    }

    #[test]
    fn aggregate_release_notes_with_previous_install_includes_only_current_release() {
        let notes = cumulative_release_notes();

        let aggregated = aggregate_release_notes(notes, Some("0.0.20"));

        assert_eq!(aggregated.sections.len(), 2);
        assert_eq!(aggregated.sections[0].items, vec!["Added 0.0.21 feature."]);
        assert_eq!(aggregated.sections[1].items, vec!["Fixed 0.0.21 issue."]);
    }

    #[test]
    fn aggregate_release_notes_without_history_preserves_current_release() {
        let notes = release_notes();

        let aggregated = aggregate_release_notes(notes, Some("1.2.0"));

        assert_eq!(aggregated.sections.len(), 1);
        assert_eq!(aggregated.sections[0].items, vec!["Fixed update handling."]);
    }

    #[test]
    fn progress_screen_with_release_notes_keeps_notes_visible() {
        let notes = release_notes();

        let screen = progress_screen(Some(&notes));

        assert!(screen.contains("Preparing update"));
        assert!(screen.contains("Patch Notes"));
        assert!(screen.contains("Fixed update handling."));
    }

    #[test]
    fn render_escapes_status_text() {
        let screen = render("Status", false, "<script>alert('gate')</script>", "");

        assert!(screen.contains("&lt;script&gt;alert(&#39;gate&#39;)&lt;/script&gt;"));
        assert!(!screen.contains("<script>alert('gate')</script>"));
    }

    #[test]
    fn render_with_full_window_layout_omits_inner_frame() {
        let screen = render("Status", false, "Ready.", "");
        let initial_screen = include_str!("../../ui/index.html");

        assert!(screen.contains(".card{position:relative;width:100%;height:100%"));
        assert!(!screen.contains("width:min(94vw,440px)"));
        assert!(!screen.contains("background:rgba(16,22,43,.72)"));
        assert!(!screen.contains("border:1px solid rgba(120,160,255,.18)"));
        assert!(!screen.contains("border-radius:18px"));
        assert!(!screen.contains("backdrop-filter:blur(14px)"));
        assert!(!screen.contains("box-shadow:0 30px 80px"));
        assert!(initial_screen.contains("width: 100%; height: 100%"));
        assert!(!initial_screen.contains("width: min(94vw, 440px)"));
        assert!(!initial_screen.contains("background: rgba(16, 22, 43, 0.72)"));
        assert!(!initial_screen.contains("border: 1px solid rgba(120, 160, 255, 0.18)"));
        assert!(!initial_screen.contains("border-radius: 18px"));
        assert!(!initial_screen.contains("backdrop-filter: blur(14px)"));
        assert!(!initial_screen.contains("box-shadow: 0 30px 80px"));
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
    fn linux_data_dir_with_xdg_home_uses_configured_directory() {
        assert_eq!(
            linux_data_dir(Path::new("/home/player"), Some(Path::new("/data"))),
            Path::new("/data/rebellion2")
        );
    }

    #[test]
    fn linux_data_dir_without_xdg_home_uses_local_share() {
        assert_eq!(
            linux_data_dir(Path::new("/home/player"), None),
            Path::new("/home/player/.local/share/rebellion2")
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

    fn release_notes() -> ReleaseNotes {
        ReleaseNotes {
            version: "1.2.3".to_string(),
            sections: vec![ReleaseNoteSection {
                title: "Fixes".to_string(),
                items: vec!["Fixed update handling.".to_string()],
            }],
            releases: Vec::new(),
        }
    }

    fn cumulative_release_notes() -> ReleaseNotes {
        ReleaseNotes {
            version: "0.0.21".to_string(),
            sections: vec![
                ReleaseNoteSection {
                    title: "Additions".to_string(),
                    items: vec!["Added 0.0.21 feature.".to_string()],
                },
                ReleaseNoteSection {
                    title: "Fixes".to_string(),
                    items: vec!["Fixed 0.0.21 issue.".to_string()],
                },
            ],
            releases: vec![
                VersionedReleaseNotes {
                    version: "0.0.15".to_string(),
                    sections: vec![ReleaseNoteSection {
                        title: "Fixes".to_string(),
                        items: vec!["Fixed installed issue.".to_string()],
                    }],
                },
                VersionedReleaseNotes {
                    version: "0.0.16".to_string(),
                    sections: vec![
                        ReleaseNoteSection {
                            title: "Additions".to_string(),
                            items: vec!["Added 0.0.16 feature.".to_string()],
                        },
                        ReleaseNoteSection {
                            title: "Fixes".to_string(),
                            items: vec!["Fixed 0.0.16 issue.".to_string()],
                        },
                    ],
                },
                VersionedReleaseNotes {
                    version: "0.0.17".to_string(),
                    sections: vec![ReleaseNoteSection {
                        title: "Changes".to_string(),
                        items: vec!["Changed 0.0.17 behavior.".to_string()],
                    }],
                },
                VersionedReleaseNotes {
                    version: "0.0.21".to_string(),
                    sections: vec![
                        ReleaseNoteSection {
                            title: "Additions".to_string(),
                            items: vec!["Added 0.0.21 feature.".to_string()],
                        },
                        ReleaseNoteSection {
                            title: "Fixes".to_string(),
                            items: vec!["Fixed 0.0.21 issue.".to_string()],
                        },
                    ],
                },
            ],
        }
    }
}
