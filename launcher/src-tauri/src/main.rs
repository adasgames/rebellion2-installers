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

mod content_packs;
mod downloads;
mod manifests;
mod presentation;
mod release_notes;
mod versions;

#[cfg(any(target_os = "windows", test))]
use manifests::validate_application_manifest;
#[cfg(target_os = "macos")]
use manifests::validate_macos_game_archive_manifest;
use manifests::{
    is_base_content_path, validate_base_content_manifest, validate_base_content_release_manifest,
    validate_game_manifest, validate_launcher_manifest,
};

#[cfg(test)]
use presentation::update_check_failure_reason;
use presentation::{
    application_update_screen, content_pack_error_screen, game_update_screen,
    launcher_staged_ready_screen, launcher_update_screen, operation_failure_status,
    progress_screen, required_content_pack_update_screen, required_update_signin_screen,
    retryable_failure_screen, up_to_date_screen, update_available_screen,
    update_check_failed_screen, update_signin_screen, verification_failure_screen,
};

use presentation::{
    button, disabled_button, html_escape, render, render_with_content, secondary_button,
};
#[cfg(any(debug_assertions, test))]
use release_notes::ReleaseNoteSection;
#[cfg(test)]
use release_notes::VersionedReleaseNotes;
use release_notes::{
    aggregate_release_notes, parse_release_notes, ReleaseNotes, ReleaseNotesPointer,
};
use versions::version_gt;
#[cfg(test)]
use versions::{compare_versions, parse_semantic_version};

#[cfg(test)]
use std::cmp::Ordering as VersionOrdering;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use content_packs::{ContentPackCatalog, ContentPackRelease, GAME_DEPENDENCY_ID};
use downloads::{
    clear_cache, clear_download_metadata, download_to_path, known_download_size, DownloadError,
    DownloadSpec,
};
#[cfg(test)]
use rebellion2_update_core::sha256_hex;
use rebellion2_update_core::FileEntry;
#[cfg(target_os = "windows")]
use rebellion2_update_core::Plan;
use rebellion2_update_core::{
    apply, apply_verified_with_metadata, apply_with_metadata, diff, diff_verified,
    recover_interrupted_apply, replace_file_atomically, sha256_file, store_metadata,
    verify_install, BlobSource, InstallationLock, Manifest,
};
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
/// Public base URL of the optional content-pack channel (REB2_CONTENT_PACK_BASE_URL).
const CONTENT_PACK_BASE: Option<&str> = option_env!("REB2_CONTENT_PACK_BASE_URL");

const CONTENT_VERSION_FILE: &str = ".content-version";
const CONTENT_MANIFEST_FILE: &str = ".manifest.json";
const CONTENT_VERIFICATION_FILE: &str = ".verified-manifest.sha256";
const CONTENT_INSTALL_STAGING_DIRECTORY: &str = ".content-install-staging";
const APPROVED_CONTENT_UPDATE_FILE: &str = ".approved-content-update";
const LAUNCHER_MANIFEST_FILE: &str = ".launcher-manifest.json";
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
#[cfg(any(target_os = "windows", test))]
const WINDOWS_NATIVE_PLUGIN_DIRECTORY: &str = "Rebellion2_Data/Plugins/x86_64";
#[cfg(target_os = "linux")]
const GAME_EXE: &str = "Rebellion2.x86_64";
#[cfg(target_os = "linux")]
const UNITY_CRASH_HANDLER_EXE: &str = "UnityCrashHandler64";

#[cfg(target_os = "macos")]
const MACOS_GAME_APP_NAME: &str = "Rebellion2 Game.app";

const RELEASES_URL: &str = "https://github.com/adasgames/rebellion2-installers/releases/latest";
/// Steam's settings page for the account signed in to the player's browser.
const STEAM_PRIVACY_URL: &str = "https://steamcommunity.com/my/edit/settings";

/// Internal URL scheme the in-window buttons navigate to; intercepted in on_nav so
/// the HTML can drive Rust without extra IPC wiring.
const ACT: &str = "https://launcher.invalid/act";

/// The current session token + the pending action, set once we know what to do and
/// read when the user clicks a button in the webview.
static SESSION: Mutex<Option<String>> = Mutex::new(None);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// A newer independent launcher release waiting for approval.
static PENDING_LAUNCHER_UPDATE: Mutex<Option<PendingUpdate<LauncherUpdate>>> = Mutex::new(None);
/// A newer game-player release waiting for approval.
static PENDING_GAME_UPDATE: Mutex<Option<PendingUpdate<GameUpdate>>> = Mutex::new(None);
/// The last validated optional-pack catalog shown by the launcher.
static CONTENT_PACK_CATALOG: Mutex<Option<ContentPackCatalog>> = Mutex::new(None);
/// Set after this launcher session downloads an update for its next start.
static LAUNCHER_UPDATE_STAGED: AtomicBool = AtomicBool::new(false);
/// A newer application release waiting for the user to approve its installation.
#[cfg(target_os = "windows")]
static PENDING_APPLICATION_UPDATE: Mutex<Option<PendingUpdate<ApplicationUpdate>>> =
    Mutex::new(None);
/// The macOS application release waiting for the user to approve its installation.
#[cfg(target_os = "macos")]
static PENDING_APPLICATION_VERSION: Mutex<Option<PendingUpdate<String>>> = Mutex::new(None);
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
/// The failure screen waiting for the ownership webview to return to bundled content.
static GATE_FAILURE_PENDING: Mutex<Option<String>> = Mutex::new(None);
/// Set until the bundled launcher page is ready for its first channel scan.
static INITIAL_SCAN_PENDING: AtomicBool = AtomicBool::new(false);
/// Prevents overlapping installer operations from sharing transaction and download state.
static INSTALL_OPERATION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
/// Enables the local, network-free split-update walkthrough in debug builds.
#[cfg(debug_assertions)]
static PREVIEW_UPDATE_FLOW: AtomicBool = AtomicBool::new(false);

/// Keeps an offered release and its displayed notes together so approving it needs no extra requests.
#[derive(Clone)]
struct PendingUpdate<T> {
    release: T,
    notes: Option<ReleaseNotes>,
}

struct InstallOperationGuard;

impl Drop for InstallOperationGuard {
    fn drop(&mut self) {
        INSTALL_OPERATION_IN_PROGRESS.store(false, Ordering::Release);
    }
}

/// Acquires exclusive access to all on-disk installation and update state.
fn begin_install_operation(handle: &tauri::AppHandle) -> Option<InstallOperationGuard> {
    if INSTALL_OPERATION_IN_PROGRESS
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        update_status(
            handle,
            "Another installation operation is already in progress.",
        );
        return None;
    }
    Some(InstallOperationGuard)
}

/// Ed25519 public key (hex) that must have signed executable update manifests.
#[cfg(not(debug_assertions))]
const APPLICATION_UPDATE_PUBKEY: &str =
    "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d";

/// Allows debug builds to exercise signed updates against a local fixture service.
#[cfg(debug_assertions)]
const APPLICATION_UPDATE_PUBKEY: &str = match option_env!("REB2_TEST_UPDATE_PUBKEY") {
    Some(key) => key,
    None => "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d",
};

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
        progress_start: u8,
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

fn default_blobs() -> String {
    "blobs/".to_string()
}

fn main() {
    let _installation = match install_dir()
        .and_then(|directory| InstallationLock::try_acquire(&directory))
    {
        Ok(installation) => installation,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::WouldBlock {
                "The launcher is already open or finishing an update. Close the other launcher and try again.".to_string()
            } else {
                format!("The installation could not be opened: {error}")
            };
            show_startup_error(message);
            return;
        }
    };
    log_line(&format!(
        "[launcher] Session started (version {}, process {}).",
        LAUNCHER_VERSION.unwrap_or(env!("CARGO_PKG_VERSION")),
        std::process::id()
    ));
    #[cfg(debug_assertions)]
    PREVIEW_UPDATE_FLOW.store(
        std::env::args().any(|argument| argument == "--preview-update-flow"),
        Ordering::Relaxed,
    );
    #[cfg(debug_assertions)]
    let preview_update_flow = PREVIEW_UPDATE_FLOW.load(Ordering::Relaxed);
    #[cfg(not(debug_assertions))]
    let preview_update_flow = false;

    let interrupted_updates_ready = preview_update_flow
        || match recover_interrupted_updates() {
            Ok(0) => true,
            Ok(recovered) => {
                log_line(&format!(
                    "[launcher] restored {recovered} interrupted update layer(s) before startup."
                ));
                true
            }
            Err(error) => {
                log_line(&format!(
                    "[launcher] couldn't restore an interrupted update before startup: {error}"
                ));
                false
            }
        };
    if !preview_update_flow && interrupted_updates_ready && hand_off_pending_launcher_update() {
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
                // failure can still offer the installed game when its content matches.
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

/// Displays startup failures without reading, recovering, or modifying the locked installation.
fn show_startup_error(message: String) {
    let result = tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            let navigation_handle = handle.clone();
            let screen = render(
                "Launcher unavailable",
                false,
                &message,
                &button("Close", "quit"),
            );
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Rebellion 2 Launcher")
                .inner_size(520.0, 700.0)
                .resizable(false)
                .on_page_load(move |_, payload| {
                    if payload.event() == PageLoadEvent::Finished {
                        write_screen(&handle, &screen);
                    }
                })
                .on_navigation(move |url| {
                    if url.as_str().starts_with(ACT) {
                        navigation_handle.exit(0);
                        return false;
                    }
                    true
                })
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(error) = result {
        eprintln!("Could not display launcher startup error: {error}");
    }
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
        let reason = query(url, "reason");
        let profile_visibility = query(url, "profile_visibility");
        on_gate_result(
            handle,
            ok,
            message,
            presigned,
            token,
            reason,
            profile_visibility,
        );
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

    if let Some(screen) = GATE_FAILURE_PENDING.lock().unwrap().take() {
        write_screen(handle, &screen);
    } else if GATE_RETURN_PENDING.swap(false, Ordering::Relaxed) {
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
    reason: Option<String>,
    profile_visibility: Option<String>,
) {
    if !ok {
        log_line("[launcher] Ownership verification was not completed.");
        let screen = verification_failure_screen(
            message.as_deref(),
            reason.as_deref(),
            profile_visibility.as_deref(),
        );
        *GATE_FAILURE_PENDING.lock().unwrap() = Some(screen);
        let handle = handle.clone();
        thread::spawn(move || {
            if !navigate_to_local_app(&handle) {
                if let Some(screen) = GATE_FAILURE_PENDING.lock().unwrap().take() {
                    write_screen(&handle, &screen);
                }
            }
        });
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
                progress_start,
                release_notes,
            }) => Some((
                base.clone(),
                latest.clone(),
                *finishes_application_update,
                *progress_start,
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

    let first_install = match PENDING.lock().unwrap().clone() {
        Some(Pending::FirstInstall { url }) => Some(url),
        _ => None,
    };
    if let Some(url) = first_install {
        let approved = read_approved_content_update_version();
        if approved.is_some() && approved.as_deref() == requested_content_version().as_deref() {
            start_install(handle.clone(), url);
            return;
        }
    }

    let pending_update = {
        let pending = PENDING.lock().unwrap();
        match pending.as_ref() {
            Some(Pending::Update {
                base,
                latest,
                finishes_application_update,
                progress_start,
                release_notes,
            }) => Some((
                base.clone(),
                latest.clone(),
                *finishes_application_update,
                *progress_start,
                release_notes.clone(),
            )),
            _ => None,
        }
    };

    if let Some((base, latest, finishes_application_update, progress_start, release_notes)) =
        pending_update
    {
        let Some(operation) = begin_install_operation(handle) else {
            return;
        };
        show_progress_ui_with_release_notes(handle, release_notes.as_ref());
        let handle = handle.clone();
        thread::spawn(move || {
            run_update(
                &handle,
                &base,
                &latest,
                finishes_application_update,
                progress_start,
                operation,
            )
        });
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

    if INSTALL_OPERATION_IN_PROGRESS.load(Ordering::Acquire) {
        log_line(&format!(
            "[launcher] ignored '{choice}' while an installation operation was in progress."
        ));
        return;
    }

    if let Some(id) = choice.strip_prefix("content-pack-install:") {
        start_content_pack_install(handle, id);
        return;
    }
    if let Some(id) = choice.strip_prefix("content-pack-remove:") {
        start_content_pack_remove(handle, id);
        return;
    }

    match choice {
        "play" => {
            APPLICATION_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            GAME_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            clear_approved_content_update();
            if show_required_content_pack_update(handle) {
                return;
            }
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
                        Some(("Retry", "play")),
                    );
                }
            }
        }
        "content-packs" => show_content_pack_manager(handle),
        "back-to-ready" => {
            let version = install_dir()
                .ok()
                .and_then(|directory| read_installed_version(&directory.join("Content")))
                .unwrap_or_else(|| "installed".to_string());
            show_up_to_date(handle, &version);
        }
        "quit" => handle.exit(0),
        "launcher-update" => start_launcher_update(handle),
        "skip-launcher-update" => {
            *PENDING_LAUNCHER_UPDATE.lock().unwrap() = None;
            show_status_page(handle);
            let handle = handle.clone();
            thread::spawn(move || scan_game_and_content(&handle));
        }
        "application-update" => start_application_update(handle),
        "game-update" => start_game_update(handle),
        "skip-game-update" => {
            GAME_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            GAME_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            *PENDING_GAME_UPDATE.lock().unwrap() = None;
            show_status_page(handle);
            let handle = handle.clone();
            thread::spawn(move || scan_content_and_prompt(&handle));
        }
        "skip-application-update" => {
            APPLICATION_UPDATE_AUTH_PENDING.store(false, Ordering::Relaxed);
            clear_approved_content_update();
            APPLICATION_UPDATE_DISMISSED.store(true, Ordering::Relaxed);
            show_status_page(handle);
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
                progress_start,
                release_notes,
            }) = pending
            {
                let Some(operation) = begin_install_operation(handle) else {
                    return;
                };
                show_progress_ui_with_release_notes(handle, release_notes.as_ref());
                let handle = handle.clone();
                thread::spawn(move || {
                    run_update(
                        &handle,
                        &base,
                        &latest,
                        finishes_application_update,
                        progress_start,
                        operation,
                    )
                });
            } else {
                show_status_page(handle);
                let handle = handle.clone();
                thread::spawn(move || scan_and_prompt(&handle));
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
        "steam-privacy" => {
            let handle = handle.clone();
            thread::spawn(move || {
                if open::that(STEAM_PRIVACY_URL).is_err() {
                    update_status(&handle,
                        "Could not open your browser. In Steam, go to Edit Profile > Privacy Settings.",
                    );
                }
            });
        }
        "steam-retry" => {
            if let Some(window) = handle.get_webview_window("main") {
                let target = steam_signin_url(&auth_base(), requested_content_version().as_deref());
                if let Ok(url) = target.parse() {
                    let _ = window.navigate(url);
                }
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
        *PENDING_LAUNCHER_UPDATE.lock().unwrap() = Some(PendingUpdate {
            release: update,
            notes: None,
        });
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
    let Some(PendingUpdate {
        release: update,
        notes,
    }) = PENDING_LAUNCHER_UPDATE.lock().unwrap().clone()
    else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    show_progress_ui_with_release_notes(handle, notes.as_ref());
    let handle = handle.clone();
    thread::spawn(move || {
        let result = do_launcher_update(&handle, &update);
        drop(operation);
        match result {
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
                show_retryable_failure(
                    &handle,
                    "Update failed",
                    operation_failure_status(error.as_ref()),
                    "launcher-update",
                    Some(("Continue", "skip-launcher-update")),
                );
            }
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
    let manifest_bytes = fetch_channel_bytes(&format!("{base}{}", layer.manifest), None)?;
    verify_signed_manifest(&manifest_bytes, &layer.signature)?;
    let manifest = Manifest::from_json(&manifest_bytes)?;
    let (launcher_entry, helper_entry) = validate_launcher_manifest(&manifest, &update.version)?;
    let blobs = HttpBlobs::new(
        format!("{base}{}", layer.blobs),
        None,
        handle,
        launcher_entry.size.saturating_add(helper_entry.size),
        0..=95,
        "Downloading launcher update",
        "launcher",
    )?;
    update_status(handle, "Downloading launcher update…");
    let metadata_dir = install_dir()?;
    let plan = rebellion2_update_core::Plan {
        changed: vec![
            FileEntry {
                path: UPDATE_HELPER_FILE_NAME.to_string(),
                ..helper_entry.clone()
            },
            FileEntry {
                path: STAGED_LAUNCHER_FILE_NAME.to_string(),
                ..launcher_entry.clone()
            },
        ],
        removed: {
            #[cfg(target_os = "windows")]
            {
                vec![
                    PENDING_APPLICATION_MANIFEST_FILE.to_string(),
                    PENDING_APPLICATION_VERSION_FILE.to_string(),
                ]
            }
            #[cfg(not(target_os = "windows"))]
            {
                Vec::new()
            }
        },
    };
    apply_with_metadata(
        &plan,
        &metadata_dir,
        &blobs,
        &[
            (PENDING_LAUNCHER_MANIFEST_FILE, &manifest_bytes),
            (PENDING_LAUNCHER_VERSION_FILE, update.version.as_bytes()),
        ],
    )?;
    ensure_executable(&metadata_dir.join(UPDATE_HELPER_FILE_NAME))?;
    ensure_executable(&metadata_dir.join(STAGED_LAUNCHER_FILE_NAME))?;
    blobs.clear_cache();
    Ok(())
}

fn write_file_atomically(path: PathBuf, bytes: &[u8]) -> io::Result<()> {
    let temporary = path_with_suffix(&path, ".writing");
    remove_path_if_present(&temporary)?;
    fs::write(&temporary, bytes)?;
    replace_file_atomically(&temporary, &path)
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
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
        *PENDING_GAME_UPDATE.lock().unwrap() = Some(PendingUpdate {
            release: update,
            notes: None,
        });
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
    let Some(PendingUpdate {
        release: update,
        notes,
    }) = PENDING_GAME_UPDATE.lock().unwrap().clone()
    else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    if SESSION.lock().unwrap().is_none() {
        GAME_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
        show_update_signin(handle, notes.as_ref());
        return;
    }
    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    show_progress_ui_with_release_notes(handle, notes.as_ref());
    let handle = handle.clone();
    thread::spawn(move || {
        run_game_update(&handle, &update, notes, operation);
    });
}

/// Applies a signed game-player layer before updating its matching protected content.
fn run_game_update(
    handle: &tauri::AppHandle,
    update: &GameUpdate,
    release_notes: Option<ReleaseNotes>,
    operation: InstallOperationGuard,
) {
    match do_game_update(handle, update) {
        Ok((changed, download_size)) => {
            log_line(&format!(
                "[launcher] game update to {} complete ({changed} files).",
                update.version
            ));
            let progress_start = if download_size > 0 { 48 } else { 0 };
            let Some(base) = content_base() else {
                drop(operation);
                show_message(
                    handle,
                    "Update failed",
                    "The game content channel is not configured.",
                    None,
                );
                return;
            };
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: update.content.clone(),
                finishes_application_update: true,
                progress_start,
                release_notes,
            });
            run_update(
                handle,
                &base,
                &update.content,
                true,
                progress_start,
                operation,
            );
        }
        Err(error) => {
            drop(operation);
            log_line(&format!("[launcher] game update failed: {error}"));
            show_retryable_failure(
                handle,
                "Update failed",
                operation_failure_status(error.as_ref()),
                "game-update",
                Some(("Continue", "skip-game-update")),
            );
        }
    }
}

/// Downloads and applies only files owned by the game player.
fn do_game_update(
    handle: &tauri::AppHandle,
    update: &GameUpdate,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    let layer = game_layer(update).ok_or("game release does not support this platform")?;
    let base = content_base().ok_or("game update channel is not configured")?;
    let manifest_bytes = fetch_channel_bytes(&format!("{base}{}", layer.manifest), None)?;
    verify_signed_manifest(&manifest_bytes, &layer.signature)?;
    let remote = Manifest::from_json(&manifest_bytes)?;
    validate_game_manifest(&remote, &update.version)?;
    let metadata_dir = install_dir()?;
    write_approved_content_update(&metadata_dir, &update.version)?;

    #[cfg(target_os = "macos")]
    let (changed, download_size) = {
        let archive = validate_macos_game_archive_manifest(&remote)?;
        update_status(
            handle,
            &format!("Updating game… {}", human_bytes(archive.size)),
        );
        let blobs = HttpBlobs::new(
            format!("{base}{}", layer.blobs),
            None,
            handle,
            archive.size,
            0..=48,
            "Updating game",
            "game",
        )?;
        let archive_path = blobs.download(archive)?;
        install_macos_game_archive(&archive_path)?;
        (1, archive.size)
    };

    #[cfg(not(target_os = "macos"))]
    let (changed, download_size) = {
        let game_dir = install_dir()?;
        let local = match read_game_manifest(&metadata_dir) {
            Some(manifest) => manifest,
            None => snapshot_application_files(&game_dir, &remote)?,
        };
        let plan = diff_verified(Some(&local), &remote, &game_dir)?;
        #[cfg(target_os = "windows")]
        let (plan, plugin_removals) = {
            let mut plan = plan;
            let removed = include_unlisted_windows_native_plugins(&game_dir, &remote, &mut plan)?;
            (plan, removed)
        };
        let download_size = plan.download_size();
        update_status(
            handle,
            &format!("Updating game… {}", human_bytes(download_size)),
        );
        let changed = plan.changed.len();
        let blobs = HttpBlobs::new(
            format!("{base}{}", layer.blobs),
            None,
            handle,
            download_size,
            0..=48,
            "Updating game",
            "game",
        )?;
        apply_verified_with_metadata(
            &plan,
            &game_dir,
            &blobs,
            &[
                (GAME_MANIFEST_FILE, &manifest_bytes),
                (GAME_VERSION_FILE, update.version.as_bytes()),
            ],
            &remote,
        )?;
        #[cfg(target_os = "windows")]
        {
            if plugin_removals > 0 {
                log_line(&format!(
                    "[launcher] removed {plugin_removals} unlisted native game plugin(s)."
                ));
            }
        }
        #[cfg(target_os = "linux")]
        ensure_linux_player_executables(&game_dir)?;
        (changed, download_size)
    };

    #[cfg(target_os = "macos")]
    apply_with_metadata(
        &rebellion2_update_core::Plan::default(),
        &metadata_dir,
        &EmptyBlobs,
        &[
            (GAME_MANIFEST_FILE, &manifest_bytes),
            (GAME_VERSION_FILE, update.version.as_bytes()),
        ],
    )?;
    clear_download_cache("game");
    Ok((changed, download_size))
}

/// Replaces the nested macOS game bundle while keeping a rollback copy.
#[cfg(target_os = "macos")]
fn install_macos_game_archive(archive_path: &Path) -> io::Result<()> {
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
        .arg(archive_path)
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
        .filter(|manifest| validate_game_manifest(manifest, &manifest.version).is_ok())
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
        .or_else(|| {
            #[cfg(target_os = "windows")]
            {
                pending_application_game_version()
            }
            #[cfg(not(target_os = "windows"))]
            {
                None
            }
        })
        .or_else(current_application_version)
}

/// Reads the player version already committed by a combined update before launcher promotion.
#[cfg(target_os = "windows")]
fn pending_application_game_version() -> Option<String> {
    let directory = install_dir().ok()?;
    let version = fs::read_to_string(directory.join(PENDING_APPLICATION_VERSION_FILE)).ok()?;
    let manifest =
        Manifest::from_json(&fs::read(directory.join(PENDING_APPLICATION_MANIFEST_FILE)).ok()?)
            .ok()?;
    validate_application_manifest(&manifest, version.trim()).ok()?;
    Some(version.trim().to_string())
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
    let current = current_game_version()?;
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
        *PENDING_APPLICATION_UPDATE.lock().unwrap() = Some(PendingUpdate {
            release: update,
            notes: None,
        });
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
            *PENDING_APPLICATION_VERSION.lock().unwrap() = Some(PendingUpdate {
                release: version.clone(),
                notes: None,
            });
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
    if let Some(PendingUpdate {
        release: update,
        notes,
    }) = update
    {
        if !begin_application_update_or_request_authorization(handle, notes.as_ref()) {
            return;
        }
        let Some(operation) = begin_install_operation(handle) else {
            return;
        };
        show_progress_ui_with_release_notes(handle, notes.as_ref());
        let handle = handle.clone();
        thread::spawn(move || {
            run_windows_application_update(&handle, &update, notes, operation);
        });
    }
}

#[cfg(target_os = "macos")]
fn start_application_update(handle: &tauri::AppHandle) {
    let version = PENDING_APPLICATION_VERSION.lock().unwrap().clone();
    let Some(PendingUpdate { notes, .. }) = version else {
        show_status_page(handle);
        let handle = handle.clone();
        thread::spawn(move || scan_and_prompt(&handle));
        return;
    };
    if !begin_application_update_or_request_authorization(handle, notes.as_ref()) {
        return;
    }
    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    show_progress_ui_with_release_notes(handle, notes.as_ref());
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        run_macos_application_update(&handle, notes, operation).await;
    });
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn start_application_update(_handle: &tauri::AppHandle) {}

/// Returns whether protected content can be downloaded, or opens ownership verification.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn begin_application_update_or_request_authorization(
    handle: &tauri::AppHandle,
    notes: Option<&ReleaseNotes>,
) -> bool {
    if SESSION.lock().unwrap().is_some() {
        return true;
    }

    APPLICATION_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
    show_update_signin(handle, notes);
    false
}

#[cfg(target_os = "macos")]
async fn run_macos_application_update(
    handle: &tauri::AppHandle,
    release_notes: Option<ReleaseNotes>,
    operation: InstallOperationGuard,
) {
    update_progress(handle, 0, "Preparing update\u{2026}");
    let updater = match handle.updater() {
        Ok(updater) => updater,
        Err(error) => {
            drop(operation);
            show_macos_application_update_error(handle, &error);
            return;
        }
    };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            drop(operation);
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
            drop(operation);
            show_macos_application_update_error(handle, &error);
            return;
        }
    };

    let (base, latest) = match prepare_application_content(&update.version, None) {
        Ok(release) => release,
        Err(error) => {
            drop(operation);
            show_application_update_preparation_error(
                handle,
                error.as_ref(),
                release_notes.as_ref(),
            );
            return;
        }
    };

    if let Err(error) = store_approved_content_update(&update.version) {
        drop(operation);
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
                    .map(|total| scaled_download_percent(downloaded, total, 0, 48))
                    .unwrap_or(0);
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
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: latest.clone(),
                finishes_application_update: true,
                progress_start: 48,
                release_notes,
            });
            run_update(handle, &base, &latest, true, 48, operation);
        }
        Err(error) => {
            clear_approved_content_update();
            drop(operation);
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
    error: &(dyn std::error::Error + 'static),
    release_notes: Option<&ReleaseNotes>,
) {
    if is_authorization_required(error) {
        log_line("[launcher] update authorization expired before the application download.");
        clear_token();
        APPLICATION_UPDATE_AUTH_PENDING.store(true, Ordering::Relaxed);
        show_update_signin(handle, release_notes);
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

/// Installs one application/content release and leaves the launcher ready to play.
#[cfg(target_os = "windows")]
fn run_windows_application_update(
    handle: &tauri::AppHandle,
    update: &ApplicationUpdate,
    release_notes: Option<ReleaseNotes>,
    operation: InstallOperationGuard,
) {
    update_progress(handle, 0, "Preparing update…");
    let (base, latest) = match prepare_application_content(&update.version, update.content.clone())
    {
        Ok(release) => release,
        Err(error) => {
            drop(operation);
            show_application_update_preparation_error(
                handle,
                error.as_ref(),
                release_notes.as_ref(),
            );
            return;
        }
    };

    match do_application_update(handle, update) {
        Ok((changed, download_size)) => {
            log_line(&format!(
                "[launcher] application update to {} staged ({changed} files).",
                update.version
            ));
            let progress_start = if download_size > 0 { 48 } else { 0 };
            *PENDING.lock().unwrap() = Some(Pending::Update {
                base: base.clone(),
                latest: latest.clone(),
                finishes_application_update: true,
                progress_start,
                release_notes,
            });
            run_update(handle, &base, &latest, true, progress_start, operation);
        }
        Err(error) => {
            clear_approved_content_update();
            drop(operation);
            log_line(&format!("[launcher] application update failed: {error}"));
            show_retryable_failure(
                handle,
                "Update failed",
                operation_failure_status(error.as_ref()),
                "application-update",
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
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    let base = content_base().ok_or("application update channel is not configured")?;
    let manifest_bytes = fetch_channel_bytes(&format!("{base}{}", update.manifest), None)?;
    verify_signed_manifest(&manifest_bytes, &update.signature)?;
    let remote = Manifest::from_json(&manifest_bytes)?;
    validate_application_manifest(&remote, &update.version)?;

    let install_dir = install_dir()?;
    write_approved_content_update(&install_dir, &update.version)?;
    let local = match read_application_manifest(&install_dir) {
        Some(manifest) => manifest,
        None => snapshot_application_files(&install_dir, &remote)?,
    };
    let plan = diff_verified(Some(&local), &remote, &install_dir)?;
    let download_size = plan.download_size();
    update_status(
        handle,
        &format!("Updating application… {}", human_bytes(download_size)),
    );

    let blobs = HttpBlobs::new(
        format!("{base}{}", update.blobs),
        None,
        handle,
        download_size,
        0..=48,
        "Updating application",
        "application",
    )?;
    let mut immediate_plan = Plan {
        changed: plan
            .changed
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                if entry.path == LAUNCHER_FILE_NAME {
                    entry.path = STAGED_LAUNCHER_FILE_NAME.to_string();
                }
                entry
            })
            .collect(),
        removed: plan
            .removed
            .iter()
            .filter(|path| path.as_str() != LAUNCHER_FILE_NAME)
            .cloned()
            .chain([
                PENDING_LAUNCHER_MANIFEST_FILE.to_string(),
                PENDING_LAUNCHER_VERSION_FILE.to_string(),
                STAGED_LAUNCHER_FILE_NAME.to_string(),
            ])
            .collect(),
    };
    include_unlisted_windows_native_plugins(&install_dir, &remote, &mut immediate_plan)?;
    let mut expected = remote.clone();
    if immediate_plan
        .changed
        .iter()
        .any(|entry| entry.path == STAGED_LAUNCHER_FILE_NAME)
    {
        if let Some(entry) = expected
            .files
            .iter_mut()
            .find(|entry| entry.path == LAUNCHER_FILE_NAME)
        {
            entry.path = STAGED_LAUNCHER_FILE_NAME.to_string();
        }
    }
    let changed = apply_verified_with_metadata(
        &immediate_plan,
        &install_dir,
        &blobs,
        &[
            (PENDING_APPLICATION_MANIFEST_FILE, &manifest_bytes),
            (PENDING_APPLICATION_VERSION_FILE, update.version.as_bytes()),
        ],
        &expected,
    )?;
    blobs.clear_cache();
    update_status(handle, "Application files ready. Updating game content…");
    Ok((changed, download_size))
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

/// Reads the application manifest installed by setup or the previous update.
#[cfg(target_os = "windows")]
fn read_application_manifest(install_dir: &Path) -> Option<Manifest> {
    let bytes = fs::read(install_dir.join(APPLICATION_MANIFEST_FILE)).ok()?;
    Manifest::from_json(&bytes)
        .ok()
        .filter(|manifest| validate_application_manifest(manifest, &manifest.version).is_ok())
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
        let size = fs::metadata(&path)?.len();
        files.push(FileEntry {
            path: remote_file.path.clone(),
            sha256: sha256_file(&path)?,
            size,
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
    let Ok(launcher) = launcher_path() else {
        return false;
    };
    match metadata_dir.join(STAGED_LAUNCHER_FILE_NAME).try_exists() {
        Ok(false) => {
            match recover_completed_launcher_handoff(&metadata_dir, &launcher) {
                Ok(CompletedLauncherHandoffState::None) => {}
                Ok(CompletedLauncherHandoffState::Recovered) => {
                    log_line("[launcher] completed interrupted launcher metadata promotion.");
                }
                #[cfg(target_os = "windows")]
                Ok(CompletedLauncherHandoffState::WaitingForContent) => {
                    log_line(
                        "[launcher] promoted application launcher is waiting for its matching Content update.",
                    );
                }
                Ok(CompletedLauncherHandoffState::Invalid) => {
                    log_line("[launcher] discarding invalid pending launcher metadata.");
                    clear_incomplete_launcher_update(&metadata_dir);
                }
                Err(error) => log_line(&format!(
                    "[launcher] couldn't recover launcher metadata promotion: {error}"
                )),
            }
            return false;
        }
        Ok(true) => {}
        Err(error) => {
            log_line(&format!(
                "[launcher] couldn't inspect the staged launcher update: {error}"
            ));
            return false;
        }
    }
    match staged_launcher_update_state(&metadata_dir, &launcher) {
        Ok(StagedLauncherUpdateState::Ready) => {}
        Ok(StagedLauncherUpdateState::Superseded) => {
            log_line(
                "[launcher] discarding a staged launcher superseded by the installed release.",
            );
            clear_incomplete_launcher_update(&metadata_dir);
            return false;
        }
        #[cfg(target_os = "windows")]
        Ok(StagedLauncherUpdateState::WaitingForContent) => {
            log_line(
                "[launcher] staged application launcher is waiting for its matching Content update.",
            );
            return false;
        }
        Ok(StagedLauncherUpdateState::Invalid) => {
            log_line("[launcher] discarding an incomplete staged launcher update.");
            clear_incomplete_launcher_update(&metadata_dir);
            return false;
        }
        Err(error) => {
            log_line(&format!(
                "[launcher] couldn't validate the staged launcher update: {error}"
            ));
            return false;
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagedLauncherUpdateState {
    Ready,
    Superseded,
    #[cfg(target_os = "windows")]
    WaitingForContent,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletedLauncherHandoffState {
    None,
    Recovered,
    #[cfg(target_os = "windows")]
    WaitingForContent,
    Invalid,
}

/// Finishes metadata promotion when the helper already replaced the staged launcher.
fn recover_completed_launcher_handoff(
    metadata_dir: &Path,
    launcher: &Path,
) -> io::Result<CompletedLauncherHandoffState> {
    let pending_launcher_manifest = metadata_dir.join(PENDING_LAUNCHER_MANIFEST_FILE);
    let pending_launcher_version = metadata_dir.join(PENDING_LAUNCHER_VERSION_FILE);
    let has_launcher_metadata =
        pending_launcher_manifest.try_exists()? || pending_launcher_version.try_exists()?;

    #[cfg(target_os = "windows")]
    let has_application_metadata = metadata_dir
        .join(PENDING_APPLICATION_MANIFEST_FILE)
        .try_exists()?
        || metadata_dir
            .join(PENDING_APPLICATION_VERSION_FILE)
            .try_exists()?;
    #[cfg(not(target_os = "windows"))]
    let has_application_metadata = false;

    if !has_launcher_metadata && !has_application_metadata {
        return Ok(CompletedLauncherHandoffState::None);
    }
    if has_launcher_metadata && has_application_metadata {
        return Ok(CompletedLauncherHandoffState::Invalid);
    }

    if has_launcher_metadata {
        return recover_launcher_metadata(metadata_dir, launcher);
    }

    #[cfg(target_os = "windows")]
    return recover_application_metadata(metadata_dir, launcher);

    #[cfg(not(target_os = "windows"))]
    Ok(CompletedLauncherHandoffState::Invalid)
}

/// Validates the installed launcher before completing launcher-only metadata promotion.
fn recover_launcher_metadata(
    metadata_dir: &Path,
    launcher: &Path,
) -> io::Result<CompletedLauncherHandoffState> {
    let Some(manifest_bytes) = read_pending_or_installed_file(
        metadata_dir,
        PENDING_LAUNCHER_MANIFEST_FILE,
        LAUNCHER_MANIFEST_FILE,
    )?
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Some(version_bytes) = read_pending_or_installed_file(
        metadata_dir,
        PENDING_LAUNCHER_VERSION_FILE,
        LAUNCHER_VERSION_FILE,
    )?
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Ok(manifest) = Manifest::from_json(&manifest_bytes) else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Ok(version) = std::str::from_utf8(&version_bytes) else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Ok((launcher_entry, helper_entry)) = validate_launcher_manifest(&manifest, version.trim())
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let helper = metadata_dir.join(UPDATE_HELPER_FILE_NAME);
    if !file_matches_entry_or_missing(launcher, launcher_entry)?
        || !file_matches_entry_or_missing(&helper, helper_entry)?
    {
        return Ok(CompletedLauncherHandoffState::Invalid);
    }

    promote_pending_metadata(
        metadata_dir,
        PENDING_LAUNCHER_MANIFEST_FILE,
        LAUNCHER_MANIFEST_FILE,
    )?;
    promote_pending_metadata(
        metadata_dir,
        PENDING_LAUNCHER_VERSION_FILE,
        LAUNCHER_VERSION_FILE,
    )?;
    Ok(CompletedLauncherHandoffState::Recovered)
}

/// Validates a promoted legacy launcher before completing application metadata promotion.
#[cfg(target_os = "windows")]
fn recover_application_metadata(
    metadata_dir: &Path,
    launcher: &Path,
) -> io::Result<CompletedLauncherHandoffState> {
    let Some(manifest_bytes) = read_pending_or_installed_file(
        metadata_dir,
        PENDING_APPLICATION_MANIFEST_FILE,
        APPLICATION_MANIFEST_FILE,
    )?
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Some(version_bytes) = read_pending_or_installed_file(
        metadata_dir,
        PENDING_APPLICATION_VERSION_FILE,
        APPLICATION_VERSION_FILE,
    )?
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Ok(manifest) = Manifest::from_json(&manifest_bytes) else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Ok(version) = std::str::from_utf8(&version_bytes) else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    if validate_application_manifest(&manifest, version.trim()).is_err() {
        return Ok(CompletedLauncherHandoffState::Invalid);
    }
    let Some(launcher_entry) = manifest
        .files
        .iter()
        .find(|entry| entry.path == LAUNCHER_FILE_NAME)
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let Some(helper_entry) = manifest
        .files
        .iter()
        .find(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
    else {
        return Ok(CompletedLauncherHandoffState::Invalid);
    };
    let helper = metadata_dir.join(UPDATE_HELPER_FILE_NAME);
    if !file_matches_entry_or_missing(launcher, launcher_entry)?
        || !file_matches_entry_or_missing(&helper, helper_entry)?
    {
        return Ok(CompletedLauncherHandoffState::Invalid);
    }
    if read_installed_version(&metadata_dir.join("Content")).as_deref() != Some(version.trim()) {
        return Ok(CompletedLauncherHandoffState::WaitingForContent);
    }

    promote_pending_metadata(
        metadata_dir,
        PENDING_APPLICATION_MANIFEST_FILE,
        APPLICATION_MANIFEST_FILE,
    )?;
    promote_pending_metadata(
        metadata_dir,
        PENDING_APPLICATION_VERSION_FILE,
        APPLICATION_VERSION_FILE,
    )?;
    Ok(CompletedLauncherHandoffState::Recovered)
}

/// Reads pending metadata first and falls back to an already promoted counterpart.
fn read_pending_or_installed_file(
    metadata_dir: &Path,
    pending_name: &str,
    installed_name: &str,
) -> io::Result<Option<Vec<u8>>> {
    for path in [
        metadata_dir.join(pending_name),
        metadata_dir.join(installed_name),
    ] {
        match fs::read(path) {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Promotes one remaining pending metadata file while tolerating earlier promotion.
fn promote_pending_metadata(
    metadata_dir: &Path,
    pending_name: &str,
    installed_name: &str,
) -> io::Result<()> {
    let pending = metadata_dir.join(pending_name);
    match pending.try_exists() {
        Ok(true) => replace_file_atomically(&pending, &metadata_dir.join(installed_name)),
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Treats a missing artifact as an invalid handoff rather than a launcher I/O failure.
fn file_matches_entry_or_missing(path: &Path, entry: &FileEntry) -> io::Result<bool> {
    match file_matches_entry(path, entry) {
        Ok(matches) => Ok(matches),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Verifies every staged launcher artifact before allowing the handoff helper to run.
fn staged_launcher_update_state(
    metadata_dir: &Path,
    launcher: &Path,
) -> io::Result<StagedLauncherUpdateState> {
    let staged = metadata_dir.join(STAGED_LAUNCHER_FILE_NAME);
    let helper = metadata_dir.join(UPDATE_HELPER_FILE_NAME);
    let manifest_path = metadata_dir.join(PENDING_LAUNCHER_MANIFEST_FILE);
    let version_path = metadata_dir.join(PENDING_LAUNCHER_VERSION_FILE);
    let launcher_manifest_exists = manifest_path.try_exists()?;
    let launcher_version_exists = version_path.try_exists()?;
    if launcher_manifest_exists || launcher_version_exists {
        #[cfg(target_os = "windows")]
        if metadata_dir
            .join(PENDING_APPLICATION_MANIFEST_FILE)
            .try_exists()?
            || metadata_dir
                .join(PENDING_APPLICATION_VERSION_FILE)
                .try_exists()?
        {
            return Ok(StagedLauncherUpdateState::Invalid);
        }
        if !staged.try_exists()?
            || !helper.try_exists()?
            || !launcher_manifest_exists
            || !launcher_version_exists
        {
            return Ok(StagedLauncherUpdateState::Invalid);
        }

        let manifest = match Manifest::from_json(&fs::read(manifest_path)?) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(StagedLauncherUpdateState::Invalid),
        };
        let version = fs::read_to_string(version_path)?;
        let (launcher_entry, helper_entry) =
            match validate_launcher_manifest(&manifest, version.trim()) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    return Ok(StagedLauncherUpdateState::Invalid)
                }
                Err(error) => return Err(error),
            };
        if installed_launcher_supersedes_staged(metadata_dir, launcher, version.trim())? {
            return Ok(StagedLauncherUpdateState::Superseded);
        }
        if !file_matches_entry(&staged, launcher_entry)?
            || !file_matches_entry(&helper, helper_entry)?
        {
            return Ok(StagedLauncherUpdateState::Invalid);
        }
        ensure_executable(&staged)?;
        ensure_executable(&helper)?;
        return Ok(StagedLauncherUpdateState::Ready);
    }

    #[cfg(target_os = "windows")]
    {
        staged_application_launcher_state(metadata_dir, &staged, &helper)
    }

    #[cfg(not(target_os = "windows"))]
    Ok(StagedLauncherUpdateState::Invalid)
}

/// Preserves a verified installed release when an older staged handoff remains after reinstalling.
fn installed_launcher_supersedes_staged(
    metadata_dir: &Path,
    launcher: &Path,
    staged_version: &str,
) -> io::Result<bool> {
    let version = match fs::read_to_string(metadata_dir.join(LAUNCHER_VERSION_FILE)) {
        Ok(version) => version,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let version = version.trim();
    if versions::parse_semantic_version(version).is_none()
        || versions::parse_semantic_version(staged_version).is_none()
        || version_gt(staged_version, version)
    {
        return Ok(false);
    }
    let bytes = match fs::read(metadata_dir.join(LAUNCHER_MANIFEST_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Ok(manifest) = Manifest::from_json(&bytes) else {
        return Ok(false);
    };
    let Ok((launcher_entry, helper_entry)) = validate_launcher_manifest(&manifest, version) else {
        return Ok(false);
    };
    Ok(file_matches_entry_or_missing(launcher, launcher_entry)?
        && file_matches_entry_or_missing(
            &metadata_dir.join(UPDATE_HELPER_FILE_NAME),
            helper_entry,
        )?)
}

/// Validates the staged launcher belonging to a legacy all-application update.
#[cfg(target_os = "windows")]
fn staged_application_launcher_state(
    metadata_dir: &Path,
    staged: &Path,
    helper: &Path,
) -> io::Result<StagedLauncherUpdateState> {
    let manifest_path = metadata_dir.join(PENDING_APPLICATION_MANIFEST_FILE);
    let version_path = metadata_dir.join(PENDING_APPLICATION_VERSION_FILE);
    if !staged.try_exists()?
        || !helper.try_exists()?
        || !manifest_path.try_exists()?
        || !version_path.try_exists()?
    {
        return Ok(StagedLauncherUpdateState::Invalid);
    }

    let manifest = match Manifest::from_json(&fs::read(manifest_path)?) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(StagedLauncherUpdateState::Invalid),
    };
    let version = fs::read_to_string(version_path)?;
    if let Err(error) = validate_application_manifest(&manifest, version.trim()) {
        if error.kind() == io::ErrorKind::InvalidData {
            return Ok(StagedLauncherUpdateState::Invalid);
        }
        return Err(error);
    }
    let Some(launcher_entry) = manifest
        .files
        .iter()
        .find(|entry| entry.path == LAUNCHER_FILE_NAME)
    else {
        return Ok(StagedLauncherUpdateState::Invalid);
    };
    let Some(helper_entry) = manifest
        .files
        .iter()
        .find(|entry| entry.path == UPDATE_HELPER_FILE_NAME)
    else {
        return Ok(StagedLauncherUpdateState::Invalid);
    };
    if !file_matches_entry(staged, launcher_entry)? || !file_matches_entry(helper, helper_entry)? {
        return Ok(StagedLauncherUpdateState::Invalid);
    }
    ensure_executable(staged)?;
    ensure_executable(helper)?;

    let content_version = read_installed_version(&metadata_dir.join("Content"));
    if content_version.as_deref() != Some(version.trim()) {
        return Ok(StagedLauncherUpdateState::WaitingForContent);
    }
    Ok(StagedLauncherUpdateState::Ready)
}

fn file_matches_entry(path: &Path, entry: &FileEntry) -> io::Result<bool> {
    Ok(fs::metadata(path)?.len() == entry.size && sha256_file(path)? == entry.sha256)
}

fn clear_incomplete_launcher_update(metadata_dir: &Path) {
    for path in [
        metadata_dir.join(STAGED_LAUNCHER_FILE_NAME),
        metadata_dir.join(PENDING_LAUNCHER_MANIFEST_FILE),
        metadata_dir.join(PENDING_LAUNCHER_VERSION_FILE),
        #[cfg(target_os = "windows")]
        metadata_dir.join(PENDING_APPLICATION_MANIFEST_FILE),
        #[cfg(target_os = "windows")]
        metadata_dir.join(PENDING_APPLICATION_VERSION_FILE),
    ] {
        if let Err(error) = remove_path_if_present(&path) {
            log_line(&format!(
                "[launcher] couldn't remove incomplete update artifact {}: {error}",
                path.display()
            ));
        }
    }
}

// -- scan --------------------------------------------------------------------

/// Reads the channel pointer and decides what to show.
fn scan_and_prompt(handle: &tauri::AppHandle) {
    update_status(handle, "Checking for updates…");

    match recover_interrupted_updates() {
        Ok(0) => {}
        Ok(recovered) => log_line(&format!(
            "[launcher] restored {recovered} interrupted update layer(s)."
        )),
        Err(error) => {
            log_line(&format!(
                "[launcher] couldn't restore an interrupted update: {error}"
            ));
            show_message(
                handle,
                "Update recovery failed",
                "The previous installation could not be restored. Close the launcher and try again before starting the game.",
                Some(("Retry", "retry-update-check")),
            );
            return;
        }
    }

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
                update_status(handle, "Verifying installed content…");
                match installed_content_matches_release(&base, &latest, &content_dir) {
                    Ok(true) => {
                        refresh_content_pack_catalog_and_show_ready(handle, &latest.version)
                    }
                    Ok(false) => {
                        log_line("[launcher] installed Content does not match its release; offering repair.");
                        prompt_update(handle, &base, &latest, &content_dir);
                    }
                    Err(error) if is_authorization_required(error.as_ref()) => {
                        clear_token();
                        prompt_update(handle, &base, &latest, &content_dir);
                    }
                    Err(error) => show_update_check_failed(
                        handle,
                        installed_version.as_deref(),
                        &error.to_string(),
                    ),
                }
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
            let playable = installed_present
                && content_matches_application(
                    current_game_version().as_deref(),
                    installed_version.as_deref().unwrap_or_default(),
                );
            show_update_check_failed(
                handle,
                playable.then_some(installed_version.as_deref().unwrap_or("unknown")),
                &err.to_string(),
            );
        }
    }
}

/// Verifies installations without a completed verification record. Ordinary launches reuse a record
/// bound to the exact installed manifest; updates and explicit repairs always verify all files again.
fn installed_content_matches_release(
    base: &str,
    latest: &Latest,
    content_dir: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    if let Ok(bytes) = fs::read(content_dir.join(CONTENT_MANIFEST_FILE)) {
        if let Ok(local) = Manifest::from_json(&bytes) {
            if validate_base_content_release_manifest(&local, &latest.version).is_ok() {
                let canonical = serde_json::to_vec(&local)?;
                if fs::read_to_string(content_dir.join(CONTENT_VERIFICATION_FILE))
                    .is_ok_and(|record| record == rebellion2_update_core::sha256_hex(&canonical))
                {
                    return Ok(true);
                }
            }
        }
    }
    let token = SESSION.lock().unwrap().clone();
    let expected: Manifest = fetch_json(&format!("{base}{}", latest.manifest), token.as_deref())?;
    validate_base_content_release_manifest(&expected, &latest.version)?;
    let actual = snapshot_base_content_directory(content_dir, &latest.version, false)?;
    if !manifest_files_match(&actual, &expected) {
        return Ok(false);
    }
    let manifest = serde_json::to_vec(&expected)?;
    let verified = rebellion2_update_core::sha256_hex(&manifest);
    store_metadata(
        content_dir,
        &[
            (CONTENT_MANIFEST_FILE, &manifest),
            (CONTENT_VERIFICATION_FILE, verified.as_bytes()),
        ],
    )?;
    Ok(true)
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
        finishes_application_update: true,
        progress_start: 0,
        release_notes: release_notes.clone(),
    });

    if SESSION.lock().unwrap().is_none() {
        show_required_update_signin(handle, release_notes.as_ref());
        return;
    }

    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    show_progress_ui_with_release_notes(handle, release_notes.as_ref());
    let handle = handle.clone();
    let base = base.to_string();
    let latest = latest.clone();
    thread::spawn(move || {
        run_update(&handle, &base, &latest, true, 0, operation);
    });
}

/// Requests authorization when necessary, then computes the update size and prompts.
fn prompt_update(handle: &tauri::AppHandle, base: &str, latest: &Latest, content_dir: &Path) {
    let token = SESSION.lock().unwrap().clone();
    let release_notes = fetch_release_notes(base, latest);
    let requires_matching_content = !content_matches_application(
        current_game_version().as_deref(),
        read_installed_version(content_dir)
            .as_deref()
            .unwrap_or_default(),
    );
    *PENDING.lock().unwrap() = Some(Pending::Update {
        base: base.to_string(),
        latest: latest.clone(),
        finishes_application_update: requires_matching_content,
        progress_start: 0,
        release_notes: release_notes.clone(),
    });

    let Some(token) = token else {
        if requires_matching_content {
            show_required_update_signin(handle, release_notes.as_ref());
        } else {
            show_update_signin(handle, release_notes.as_ref());
        }
        return;
    };
    let remote: Option<Manifest> =
        match fetch_json(&format!("{base}{}", latest.manifest), Some(&token)) {
            Ok(remote) => Some(remote),
            Err(err) if is_authorization_required(err.as_ref()) => {
                clear_token();
                if requires_matching_content {
                    show_required_update_signin(handle, release_notes.as_ref());
                } else {
                    show_update_signin(handle, release_notes.as_ref());
                }
                return;
            }
            Err(_) => None,
        };
    let bytes = match &remote {
        Some(remote) => {
            let local = snapshot_base_content_directory(content_dir, "installed", false).ok();
            let plan = diff(local.as_ref(), remote);
            plan.download_size()
        }
        None => 0,
    };

    let detail = if read_installed_version(content_dir).as_deref() == Some(&latest.version) {
        format!(
            "Some installed content files need repair ({}).",
            human_bytes(bytes)
        )
    } else if bytes > 0 {
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

// -- update / install work ---------------------------------------------------

fn run_update(
    handle: &tauri::AppHandle,
    base: &str,
    latest: &Latest,
    finishes_application_update: bool,
    progress_start: u8,
    operation: InstallOperationGuard,
) {
    if let Err(error) = recover_interrupted_updates() {
        drop(operation);
        log_line(&format!(
            "[launcher] could not restore the previous update before retrying: {error}"
        ));
        show_retryable_failure(
            handle,
            "Update recovery failed",
            operation_failure_status(&error),
            "update",
            None,
        );
        return;
    }
    if install_dir().is_ok_and(|directory| !directory.join("Content/catalog.xml").is_file()) {
        drop(operation);
        request_content_archive(handle);
        return;
    }
    let result = do_update(
        handle,
        base,
        latest,
        finishes_application_update,
        u64::from(progress_start),
    );
    if result.is_ok()
        || (!finishes_application_update
            && result
                .as_ref()
                .is_err_and(|error| !is_authorization_required(error.as_ref())))
    {
        clear_approved_content_update();
    }
    if result
        .as_ref()
        .is_err_and(|error| is_authorization_required(error.as_ref()))
    {
        clear_token();
    }
    drop(operation);
    match result {
        Ok(fetched) => {
            log_line(&format!(
                "[launcher] update to {} complete ({fetched} files).",
                latest.version
            ));
            if finishes_application_update {
                finish_application_update(handle, &latest.version);
            } else {
                update_progress(handle, 100, "Update complete.");
                // Let the filled bar sit a beat, then land on the up-to-date screen.
                thread::sleep(Duration::from_millis(900));
                refresh_content_pack_catalog_and_show_ready(handle, &latest.version);
            }
        }
        Err(err) if is_content_unavailable(err.as_ref()) => {
            // A missing manifest/blob during an update means the channel is
            // unreachable or misconfigured — NOT that this version is retired.
            log_line("[launcher] update content unavailable (404 from channel).");
            show_retryable_failure(
                handle,
                "Update unavailable",
                "The update could not be downloaded. Please try again later.",
                "update",
                None,
            );
        }
        Err(err) if is_authorization_required(err.as_ref()) => {
            log_line("[launcher] update authorization expired; requesting ownership verification.");
            let release_notes = match PENDING.lock().unwrap().as_ref() {
                Some(Pending::Update { release_notes, .. }) => release_notes.clone(),
                _ => None,
            };
            if finishes_application_update {
                show_required_update_signin(handle, release_notes.as_ref());
            } else {
                show_update_signin(handle, release_notes.as_ref());
            }
        }
        Err(err) => {
            log_line(&format!("[launcher] update failed: {err}"));
            show_retryable_failure(
                handle,
                "Update interrupted",
                operation_failure_status(err.as_ref()),
                "update",
                None,
            );
        }
    }
}

/// Leaves a completed application update ready to play in the current launcher.
/// A staged Windows launcher is promoted when the user next starts the launcher.
fn finish_application_update(handle: &tauri::AppHandle, version: &str) {
    update_progress(handle, 100, "Update complete.");
    thread::sleep(Duration::from_millis(900));
    refresh_content_pack_catalog_and_show_ready(handle, version);
}

fn do_update(
    handle: &tauri::AppHandle,
    base: &str,
    latest: &Latest,
    finishes_application_update: bool,
    progress_start: u64,
) -> Result<usize, Box<dyn std::error::Error>> {
    let content_dir = install_dir()?.join("Content");
    if !content_dir.join("catalog.xml").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incremental updates require installed Content",
        )
        .into());
    }
    let token = SESSION.lock().unwrap().clone();
    let checking_label = if finishes_application_update {
        "Updating game content…"
    } else {
        "Checking what changed…"
    };
    update_status(handle, checking_label);

    let remote: Manifest = fetch_json(&format!("{base}{}", latest.manifest), token.as_deref())?;
    validate_base_content_release_manifest(&remote, &latest.version)?;
    update_status(handle, "Checking installed content files…");
    let local = snapshot_base_content_directory(&content_dir, "installed", false)?;
    let plan = diff(Some(&local), &remote);
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
    update_status(handle, &download_label);

    let progress_label = if finishes_application_update {
        "Updating game content"
    } else {
        "Downloading update"
    };
    let blobs = HttpBlobs::new(
        format!("{base}{}", latest.blobs),
        token,
        handle,
        bytes,
        progress_start..=95,
        progress_label,
        "content",
    )?;
    let manifest_json = serde_json::to_vec(&remote)?;
    let verified = rebellion2_update_core::sha256_hex(&manifest_json);
    apply_verified_with_metadata(
        &plan,
        &content_dir,
        &blobs,
        &[
            (CONTENT_MANIFEST_FILE, &manifest_json),
            (CONTENT_VERSION_FILE, latest.version.as_bytes()),
            (CONTENT_VERIFICATION_FILE, verified.as_bytes()),
        ],
        &remote,
    )?;
    blobs.clear_cache();
    Ok(changed)
}

/// Maps transferred bytes into one overall-update phase without inventing progress.
fn scaled_download_percent(done: u64, total: u64, phase_start: u64, phase_end: u64) -> u64 {
    if total == 0 {
        return phase_start;
    }
    let phase_width = phase_end.saturating_sub(phase_start);
    let phase_progress = done
        .min(total)
        .saturating_mul(phase_width)
        .checked_div(total)
        .unwrap_or(0);
    phase_start.saturating_add(phase_progress).min(phase_end)
}

/// HTTP blob source with persistent partial files and cumulative download progress.
struct HttpBlobs {
    base: String,
    token: Option<String>,
    handle: tauri::AppHandle,
    total: u64,
    done: AtomicU64,
    progress_start: u64,
    progress_end: u64,
    progress_label: String,
    cache_dir: PathBuf,
}

impl HttpBlobs {
    /// Creates one resumable source for a signed update layer.
    fn new(
        base: String,
        token: Option<String>,
        handle: &tauri::AppHandle,
        total: u64,
        progress_range: RangeInclusive<u64>,
        progress_label: &str,
        cache_name: &str,
    ) -> io::Result<Self> {
        Ok(Self {
            base,
            token,
            handle: handle.clone(),
            total,
            done: AtomicU64::new(0),
            progress_start: *progress_range.start(),
            progress_end: *progress_range.end(),
            progress_label: progress_label.to_string(),
            cache_dir: download_cache_dir(cache_name)?,
        })
    }

    /// Removes cached blobs only after the complete layer is committed.
    fn clear_cache(&self) {
        clear_committed_download_cache(&self.cache_dir);
    }
    /// Downloads or reuses a verified cached file without loading it into memory.
    fn download(&self, entry: &FileEntry) -> io::Result<PathBuf> {
        let url = format!("{}{}", self.base, entry.sha256);
        let authorization = self.token.as_ref().map(|token| format!("Bearer {token}"));
        let destination = self.cache_dir.join(&entry.sha256);
        let mut reported = 0_u64;
        let mut last_percent = u64::MAX;
        let result = download_to_path(
            &DownloadSpec {
                url: &url,
                authorization: authorization.as_deref(),
                destination: &destination,
                expected_size: Some(entry.size),
                expected_sha256: Some(&entry.sha256),
            },
            |downloaded| {
                let done = if downloaded >= reported {
                    let added = downloaded - reported;
                    self.done.fetch_add(added, Ordering::Relaxed) + added
                } else {
                    let removed = reported - downloaded;
                    let mut previous = self.done.load(Ordering::Relaxed);
                    loop {
                        let next = previous.saturating_sub(removed);
                        match self.done.compare_exchange_weak(
                            previous,
                            next,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break next,
                            Err(actual) => previous = actual,
                        }
                    }
                };
                reported = downloaded;
                let percent = scaled_download_percent(
                    done,
                    self.total,
                    self.progress_start,
                    self.progress_end,
                );
                if percent != last_percent {
                    last_percent = percent;
                    update_progress(
                        &self.handle,
                        percent,
                        &format!(
                            "{}… {} / {}",
                            self.progress_label,
                            human_bytes(done),
                            human_bytes(self.total)
                        ),
                    );
                }
            },
            |attempt, delay, error| {
                log_line(&format!(
                    "[launcher] download interrupted ({error}); retrying attempt {attempt} in {}s.",
                    delay.as_secs()
                ));
                update_status(
                    &self.handle,
                    &format!(
                        "Connection interrupted. Resuming in {} seconds…",
                        delay.as_secs()
                    ),
                );
            },
        );
        if let Err(error) = result {
            log_line(&download_failure_log_message(&url, &error.to_string()));
            if error.status() == Some(404) {
                return Err(io::Error::new(io::ErrorKind::NotFound, ContentUnavailable));
            }
            if matches!(error.status(), Some(401 | 403)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    AuthorizationRequired,
                ));
            }
            return Err(error.into_io_error());
        }
        Ok(destination)
    }
}

impl BlobSource for HttpBlobs {
    fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()> {
        fs::copy(self.download(entry)?, destination)?;
        Ok(())
    }
}

/// Downloads and extracts the full Content zip (first install), then stores the
/// version's manifest so future runs patch instead of re-pulling.
fn start_install(handle: tauri::AppHandle, url: String) {
    let Some(operation) = begin_install_operation(&handle) else {
        return;
    };
    show_progress_ui(&handle);
    thread::spawn(move || {
        let result = install_content(&handle, &url);
        drop(operation);
        match result {
            Ok(content_dir) => {
                clear_approved_content_update();
                log_line(&format!(
                    "[launcher] Content installed to {}",
                    content_dir.display()
                ));
                update_progress(&handle, 100, "Starting game…");
                match launch_game() {
                    Ok(true) => handle.exit(0),
                    Ok(false) => show_message(
                        &handle,
                        "Installed",
                        "The game is ready to play.",
                        Some(("Launch Game", "play")),
                    ),
                    Err(err) => {
                        log_line(&format!("[launcher] failed to start the game: {err}"));
                        show_message(
                            &handle,
                            "Couldn't start",
                            "The game failed to start — see launcher.log.",
                            Some(("Retry", "play")),
                        );
                    }
                }
            }
            Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
                show_update_required_ui(&handle)
            }
            Err(err)
                if is_authorization_required(err.as_ref())
                    || presigned_download_requires_authorization(err.as_ref()) =>
            {
                if is_authorization_required(err.as_ref()) {
                    clear_token();
                }
                log_line("[launcher] the Content download link expired; requesting ownership verification.");
                show_message(
                    &handle,
                    "Sign in required",
                    "Verify ownership again to refresh the download link. Your saved download progress will be reused when possible.",
                    Some(("Sign in", "signin")),
                );
            }
            Err(err) => {
                log_line(&format!("[launcher] failed to install Content: {err}"));
                show_message(
                    &handle,
                    "Download interrupted",
                    operation_failure_status(err.as_ref()),
                    Some(("Retry", "install")),
                );
            }
        }
    });
}

/// Returns whether a failed first-install request needs a fresh presigned URL.
fn presigned_download_requires_authorization(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<DownloadError>()
        .and_then(DownloadError::status)
        .is_some_and(|status| matches!(status, 401 | 403))
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

fn write_approved_content_update(directory: &Path, version: &str) -> io::Result<()> {
    let version = version.trim();
    if version.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "approved content update version is empty",
        ));
    }
    write_file_atomically(
        directory.join(APPROVED_CONTENT_UPDATE_FILE),
        version.as_bytes(),
    )
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

/// Recognizes missing manifests and blob responses through the patch engine's I/O boundary.
fn is_content_unavailable(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<ContentUnavailable>().is_some()
        || error
            .downcast_ref::<io::Error>()
            .and_then(|error| error.get_ref())
            .is_some_and(|error| error.downcast_ref::<ContentUnavailable>().is_some())
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
    normalized_base(CONTENT_BASE)
}

fn content_pack_base() -> Option<String> {
    normalized_base(CONTENT_PACK_BASE)
}

fn normalized_base(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|v| !v.is_empty()).map(|v| {
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
    let response = request.call().map_err(|err| {
        let reason = match &err {
            ureq::Error::Status(status, _) => format!("HTTP {status}"),
            _ => "connection failed".to_string(),
        };
        log_line(&download_failure_log_message(url, &reason));
        match err {
            ureq::Error::Status(404, _) => {
                Box::new(ContentUnavailable) as Box<dyn std::error::Error>
            }
            ureq::Error::Status(401 | 403, _) => {
                Box::new(AuthorizationRequired) as Box<dyn std::error::Error>
            }
            other => Box::new(other),
        }
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

fn human_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 KB".to_string();
    }
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

/// Requests a fresh authorized archive for the game version already committed to disk.
fn request_content_archive(handle: &tauri::AppHandle) {
    *PENDING.lock().unwrap() = None;
    log_line("[launcher] Content is missing; requesting its matching full archive instead of individual blobs.");
    show_status_page(handle);
    let result = gate_landing()
        .parse()
        .map_err(|error| format!("{error}"))
        .and_then(|url| {
            handle
                .get_webview_window("main")
                .ok_or_else(|| "launcher window is unavailable".to_string())?
                .navigate(url)
                .map_err(|error| error.to_string())
        });
    if let Err(error) = result {
        log_line(&format!(
            "[launcher] could not request the Content archive: {error}"
        ));
        show_message(
            handle,
            "Sign in required",
            "Verify ownership to download the matching game content.",
            Some(("Sign in", "signin")),
        );
    }
}

/// Builds a fresh Steam verification request while preserving the required content version.
fn steam_signin_url(base: &str, version: Option<&str>) -> String {
    let mut url = format!("{base}start?provider=steam");
    if let Some(version) = version {
        url.push_str(&format!("&v={}", urlencoding::encode(version)));
    }
    url
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

/// Encodes arbitrary UI text as a JavaScript string literal, including control characters.
fn js_string(text: &str) -> String {
    serde_json::to_string(text).expect("serializing a string cannot fail")
}

fn write_screen(handle: &tauri::AppHandle, html: &str) {
    let html = js_string(html);
    window_eval(
        handle,
        &format!("document.open();document.write({html});document.close();"),
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
    if show_required_content_pack_update(handle) {
        return;
    }

    let screen = if launcher_update_was_staged() {
        launcher_staged_ready_screen(version)
    } else {
        up_to_date_screen(version)
    };
    write_screen(handle, &screen);
}

/// Refreshes optional content metadata before exposing the launch action.
fn refresh_content_pack_catalog_and_show_ready(handle: &tauri::AppHandle, version: &str) {
    match fetch_content_pack_catalog() {
        Ok(catalog) => *CONTENT_PACK_CATALOG.lock().unwrap() = Some(catalog),
        Err(error) => log_line(&format!(
            "[launcher] optional content-pack update check failed: {error}"
        )),
    }
    show_up_to_date(handle, version);
}

/// Shows a blocking update action when installed optional content is behind its compatible release.
fn show_required_content_pack_update(handle: &tauri::AppHandle) -> bool {
    let Some(catalog) = CONTENT_PACK_CATALOG.lock().unwrap().clone() else {
        return false;
    };
    let Ok(install_root) = install_dir() else {
        return false;
    };
    let game_version = current_game_version();
    let Some((release, installed)) =
        pending_installed_content_pack_update(&catalog, &install_root, game_version.as_deref())
    else {
        return false;
    };

    write_screen(
        handle,
        &required_content_pack_update_screen(&release, &installed),
    );
    true
}

/// Finds the first installed optional pack with a newer compatible release.
fn pending_installed_content_pack_update(
    catalog: &ContentPackCatalog,
    install_root: &Path,
    game_version: Option<&str>,
) -> Option<(ContentPackRelease, content_packs::InstalledContentPack)> {
    catalog.packs.iter().find_map(|release| {
        let installed = content_packs::read_installed(install_root, &release.id)?;
        let minimum_supported_version = release.minimum_supported_version.as_deref()?;
        if !version_gt(minimum_supported_version, &installed.version)
            || !content_packs::supports_dependency_version(
                release,
                GAME_DEPENDENCY_ID,
                game_version,
            )
        {
            return None;
        }
        Some((release.clone(), installed))
    })
}

fn show_content_pack_manager(handle: &tauri::AppHandle) {
    write_screen(
        handle,
        &render(
            "Content packs",
            true,
            "Checking available content packs\u{2026}",
            &secondary_button("Back", "back-to-ready"),
        ),
    );
    let handle = handle.clone();
    thread::spawn(move || match fetch_content_pack_catalog() {
        Ok(catalog) => {
            *CONTENT_PACK_CATALOG.lock().unwrap() = Some(catalog.clone());
            match content_pack_manager_screen(&catalog) {
                Ok(screen) => write_screen(&handle, &screen),
                Err(error) => show_content_pack_error(&handle, "Content packs unavailable", &error),
            }
        }
        Err(error) => {
            log_line(&format!("[launcher] content-pack catalog failed: {error}"));
            show_content_pack_error(&handle, "Content packs unavailable", error.as_ref());
        }
    });
}

fn fetch_content_pack_catalog() -> Result<ContentPackCatalog, Box<dyn std::error::Error>> {
    let base = content_pack_base().ok_or("content-pack channel is not configured")?;
    let catalog: ContentPackCatalog =
        fetch_json(&format!("{base}{}", content_packs::CATALOG_PATH), None)
            .map_err(|error| content_pack_download_error("content-pack list", error))?;
    content_packs::validate_catalog(&catalog)?;
    Ok(catalog)
}

fn content_pack_manager_screen(catalog: &ContentPackCatalog) -> io::Result<String> {
    let install_root = install_dir()?;
    let game_version = current_game_version();
    Ok(content_pack_manager_screen_for(
        catalog,
        &install_root,
        game_version.as_deref(),
    ))
}

/// Renders content-pack actions against an explicit installation and game version.
fn content_pack_manager_screen_for(
    catalog: &ContentPackCatalog,
    install_root: &Path,
    game_version: Option<&str>,
) -> String {
    let mut packs = String::from("<div class=\"pack-list\">");
    if catalog.packs.is_empty() {
        packs.push_str("<p>No optional content packs are currently available.</p>");
    }
    for pack in &catalog.packs {
        let installed = content_packs::read_installed(install_root, &pack.id);
        let game_compatible =
            content_packs::supports_dependency_version(pack, GAME_DEPENDENCY_ID, game_version);
        let unavailable_content = pack
            .depends
            .iter()
            .filter(|(id, _)| id.as_str() != GAME_DEPENDENCY_ID)
            .find_map(|(id, _)| {
                let installed_version = installed_content_pack_version(install_root, id);
                (!content_packs::supports_dependency_version(
                    pack,
                    id,
                    installed_version.as_deref(),
                ))
                .then_some((id, installed_version))
            });
        let mut requirements = vec![format!(
            "Rebellion 2 {}",
            pack.depends[GAME_DEPENDENCY_ID].display()
        )];
        requirements.extend(
            pack.depends
                .iter()
                .filter(|(id, _)| id.as_str() != GAME_DEPENDENCY_ID)
                .map(|(id, requirement)| format!("{id} {}", requirement.display())),
        );
        let compatibility = format!("Requires {}", requirements.join(" · "));
        let description = if pack.description.trim().is_empty() {
            "Optional game content."
        } else {
            pack.description.trim()
        };
        let (release_status, action_label) = match installed.as_ref() {
            Some(installed) if !game_compatible => (
                match game_version {
                    Some(version) => format!(
                        "Installed {} · Incompatible with game {version}",
                        installed.version
                    ),
                    None => format!(
                        "Installed {} · Install the game to verify compatibility",
                        installed.version
                    ),
                },
                None,
            ),
            Some(installed) if unavailable_content.is_some() => (
                match unavailable_content.as_ref() {
                    Some((id, Some(version))) => format!(
                        "Installed {} · Incompatible with {id} {version}",
                        installed.version
                    ),
                    Some((id, None)) => format!(
                        "Installed {} · Required content pack {id} is missing",
                        installed.version
                    ),
                    None => unreachable!(),
                },
                None,
            ),
            Some(installed) if installed.version == pack.version => {
                (format!("Installed {}", installed.version), None)
            }
            Some(installed) => (
                format!(
                    "Installed {} · Update {} available",
                    installed.version, pack.version
                ),
                Some("Update"),
            ),
            None if !game_compatible => (
                match game_version {
                    Some(version) => {
                        format!("Requires a compatible game version · Installed {version}")
                    }
                    None => "Install the game before installing this content pack".to_string(),
                },
                None,
            ),
            None if unavailable_content.is_some() => (
                match unavailable_content.as_ref() {
                    Some((id, Some(version))) => {
                        format!("Requires compatible {id} content · Installed {version}")
                    }
                    Some((id, None)) => format!("Install required content pack {id} first"),
                    None => unreachable!(),
                },
                None,
            ),
            None => (
                format!(
                    "Version {} · {}",
                    pack.version,
                    human_bytes(pack.download_size)
                ),
                Some("Install"),
            ),
        };
        let status = format!("{release_status} · {compatibility}");
        let mut actions = String::from("<div class=\"pack-actions\">");
        if let Some(label) = action_label {
            actions.push_str(&button(label, &format!("content-pack-install:{}", pack.id)));
        }
        if installed.is_some() {
            actions.push_str(&secondary_button(
                "Remove",
                &format!("content-pack-remove:{}", pack.id),
            ));
        }
        actions.push_str("</div>");
        packs.push_str(&format!(
            "<section class=\"pack\"><h2>{}</h2><p>{}</p><p class=\"meta\">{}</p>{}</section>",
            html_escape(&pack.name),
            html_escape(description),
            html_escape(&status),
            actions
        ));
    }
    packs.push_str("</div>");
    render_with_content(
        "Content packs",
        false,
        "Install, update, or remove optional game content.",
        &packs,
        &secondary_button("Back", "back-to-ready"),
    )
}

fn start_content_pack_install(handle: &tauri::AppHandle, id: &str) {
    let pack = CONTENT_PACK_CATALOG
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|catalog| catalog.packs.iter().find(|pack| pack.id == id))
        .cloned();
    let Some(pack) = pack else {
        show_content_pack_error(
            handle,
            "Content pack unavailable",
            &io::Error::new(
                io::ErrorKind::NotFound,
                "Refresh the content-pack list and try again.",
            ),
        );
        return;
    };
    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    show_progress_ui(handle);
    update_progress(handle, 0, &format!("Preparing {}\u{2026}", pack.name));
    let handle = handle.clone();
    thread::spawn(move || {
        run_content_pack_install(&handle, &pack, operation);
    });
}

fn run_content_pack_install(
    handle: &tauri::AppHandle,
    pack: &ContentPackRelease,
    operation: InstallOperationGuard,
) {
    let result = install_content_pack(handle, pack);
    drop(operation);
    match result {
        Ok(changed) => {
            log_line(&format!(
                "[launcher] content pack {} {} installed ({changed} files).",
                pack.id, pack.version
            ));
            update_progress(handle, 100, "Content pack installed.");
            thread::sleep(Duration::from_millis(500));
            show_content_pack_manager(handle);
        }
        Err(error) => {
            log_line(&format!("[launcher] content-pack install failed: {error}"));
            show_content_pack_error(handle, "Installation failed", error.as_ref());
        }
    }
}

fn install_content_pack(
    handle: &tauri::AppHandle,
    pack: &ContentPackRelease,
) -> Result<usize, Box<dyn std::error::Error>> {
    content_packs::validate_release(pack)?;
    let game_version = current_game_version();
    let game_requirement = &pack.depends[GAME_DEPENDENCY_ID];
    if !content_packs::supports_dependency_version(
        pack,
        GAME_DEPENDENCY_ID,
        game_version.as_deref(),
    ) {
        let installed = game_version.as_deref().unwrap_or("not installed");
        return Err(format!(
            "{} requires Rebellion 2 {}; installed version is {installed}",
            pack.name,
            game_requirement.display()
        )
        .into());
    }
    let base = content_pack_base().ok_or("content-pack channel is not configured")?;
    let remote: Manifest = fetch_json(&format!("{base}{}", pack.manifest), None)
        .map_err(|error| content_pack_download_error("content-pack manifest", error))?;
    content_packs::validate_pack_manifest(&remote, pack)?;
    let install_root = install_dir()?;
    for (dependency_id, requirement) in pack
        .depends
        .iter()
        .filter(|(id, _)| id.as_str() != GAME_DEPENDENCY_ID)
    {
        let installed_dependency_version =
            installed_content_pack_version(&install_root, dependency_id);
        match installed_dependency_version {
            None => {
                return Err(
                    format!("required content pack {dependency_id} is not installed").into(),
                );
            }
            Some(installed_version)
                if !content_packs::supports_dependency_version(
                    pack,
                    dependency_id,
                    Some(&installed_version),
                ) =>
            {
                return Err(format!(
                    "{} requires content pack {dependency_id} {}; installed version is {installed_version}",
                    pack.name,
                    requirement.display()
                )
                .into());
            }
            Some(_) => {}
        }
    }
    let installed = content_packs::read_installed(&install_root, &pack.id);
    if let Some(installed) = installed.as_ref() {
        if installed.directory != pack.directory {
            return Err("content-pack install directory changed unexpectedly".into());
        }
    }
    let pack_directory = content_packs::install_directory(&install_root, pack)?;
    let local = content_packs::read_manifest(&install_root, &pack.id);
    let plan = diff_verified(
        installed.as_ref().and(local.as_ref()),
        &remote,
        &pack_directory,
    )?;
    let changed = plan.changed.len();
    let bytes = plan.download_size();
    update_status(
        handle,
        &format!("Downloading {} · {}", pack.name, human_bytes(bytes)),
    );
    let progress_label = format!("Downloading {}", pack.name);
    let blobs = HttpBlobs::new(
        format!("{base}{}", pack.blobs),
        None,
        handle,
        bytes,
        0..=95,
        &progress_label,
        &format!("content-packs/{}", pack.id),
    )?;
    if installed.is_none() {
        let changed = content_packs::install_fresh(&install_root, pack, &remote, &blobs)?;
        blobs.clear_cache();
        return Ok(changed);
    }
    content_packs::apply_update(&install_root, pack, &remote, &plan, &blobs)?;
    if !verify_install(&pack_directory, &remote)? {
        return Err("content-pack verification failed after installation".into());
    }
    blobs.clear_cache();
    Ok(changed)
}

/// Returns the installed version for the built-in pack or a launcher-managed optional pack.
fn installed_content_pack_version(install_root: &Path, id: &str) -> Option<String> {
    if id == "classic-galactic-civil-war" {
        return content_packs::read_pack_version(
            &install_root
                .join("Content")
                .join("Packs")
                .join("ClassicGalacticCivilWar")
                .join("pack.xml"),
            id,
        );
    }
    let installed = content_packs::read_installed(install_root, id)?;
    let definition_path = install_root
        .join("Content")
        .join("Packs")
        .join(&installed.directory)
        .join("pack.xml");
    content_packs::read_pack_version(&definition_path, id)
}

fn start_content_pack_remove(handle: &tauri::AppHandle, id: &str) {
    let Some(operation) = begin_install_operation(handle) else {
        return;
    };
    let id = id.to_string();
    show_progress_ui(handle);
    update_progress(handle, 10, "Removing content pack\u{2026}");
    let handle = handle.clone();
    thread::spawn(move || {
        let result = remove_content_pack(&id);
        drop(operation);
        match result {
            Ok(()) => {
                log_line(&format!("[launcher] content pack {id} removed."));
                update_progress(&handle, 100, "Content pack removed.");
                thread::sleep(Duration::from_millis(400));
                show_content_pack_manager(&handle);
            }
            Err(error) => {
                log_line(&format!("[launcher] content-pack removal failed: {error}"));
                show_content_pack_error(&handle, "Removal failed", &error);
            }
        }
    });
}

fn remove_content_pack(id: &str) -> io::Result<()> {
    let install_root = install_dir()?;
    let dependent_name = CONTENT_PACK_CATALOG
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|catalog| {
            catalog
                .packs
                .iter()
                .find(|pack| {
                    pack.depends.contains_key(id)
                        && content_packs::read_installed(&install_root, &pack.id).is_some()
                })
                .map(|pack| pack.name.clone())
        });
    if let Some(dependent_name) = dependent_name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("remove {dependent_name} first because it depends on this pack"),
        ));
    }
    let installed = content_packs::read_installed(&install_root, id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "content pack is not installed"))?;
    let manifest = content_packs::read_manifest(&install_root, id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "content-pack manifest is missing",
        )
    })?;
    let directory = install_root
        .join("Content")
        .join("Packs")
        .join(&installed.directory);
    apply(
        &content_packs::removal_plan(&installed, &manifest)?,
        &install_root,
        &EmptyBlobs,
    )?;
    if let Err(error) = remove_empty_directories(&directory) {
        log_line(&format!(
            "[launcher] unable to remove empty content-pack directories: {error}"
        ));
    }
    if let Err(error) = content_packs::remove_install_metadata(&install_root, id) {
        log_line(&format!(
            "[launcher] unable to clean removed content-pack metadata: {error}"
        ));
    }
    Ok(())
}

fn remove_empty_directories(directory: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a directory", directory.display()),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_directories(&entry.path())?;
        }
    }
    if fs::read_dir(directory)?.next().is_none() {
        fs::remove_dir(directory)?;
        return Ok(true);
    }
    Ok(false)
}

struct EmptyBlobs;

impl BlobSource for EmptyBlobs {
    fn fetch_to(&self, _entry: &FileEntry, _destination: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "a removal plan cannot download files",
        ))
    }
}

fn show_content_pack_error(
    handle: &tauri::AppHandle,
    title: &str,
    error: &(dyn std::error::Error + 'static),
) {
    write_screen(
        handle,
        &content_pack_error_screen(title, &error.to_string()),
    );
}

fn content_pack_download_error(
    resource: &str,
    error: Box<dyn std::error::Error>,
) -> Box<dyn std::error::Error> {
    if error.downcast_ref::<ContentUnavailable>().is_some() {
        format!("The {resource} could not be found on the server (HTTP 404). Try again later.")
            .into()
    } else {
        format!("Could not download the {resource}: {error}").into()
    }
}

fn download_failure_log_message(url: &str, reason: &str) -> String {
    let resource = tauri::Url::parse(url)
        .ok()
        .map(|mut url| {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|| "invalid download URL".to_string());
    format!("[launcher] Download failed: {resource} ({reason}).")
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
            .and_then(|pending| {
                fetch_release_notes_pointer(
                    &base,
                    &pending.release.version,
                    pending.release.release_notes.as_ref(),
                    current_launcher_version().as_deref(),
                )
            })
    });
    if let Some(pending) = PENDING_LAUNCHER_UPDATE.lock().unwrap().as_mut() {
        if pending.release.version == version {
            pending.notes = release_notes.clone();
        }
    }
    write_screen(
        handle,
        &launcher_update_screen(version, release_notes.as_ref()),
    );
}

/// Offers a game-player update without conflating it with a launcher release.
fn show_game_update(handle: &tauri::AppHandle, version: &str) {
    let release_notes = content_base()
        .zip(PENDING_GAME_UPDATE.lock().unwrap().clone())
        .and_then(|(base, pending)| fetch_release_notes(&base, &pending.release.content));
    if let Some(pending) = PENDING_GAME_UPDATE.lock().unwrap().as_mut() {
        if pending.release.version == version {
            pending.notes = release_notes.clone();
        }
    }
    write_screen(
        handle,
        &game_update_screen(
            version,
            release_notes.as_ref(),
            launcher_update_was_staged(),
        ),
    );
}

/// Offers an available application update without beginning its download.
fn show_application_update(handle: &tauri::AppHandle, version: &str) {
    let release_notes = fetch_current_release_notes(version);
    #[cfg(target_os = "windows")]
    if let Some(pending) = PENDING_APPLICATION_UPDATE.lock().unwrap().as_mut() {
        if pending.release.version == version {
            pending.notes = release_notes.clone();
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(pending) = PENDING_APPLICATION_VERSION.lock().unwrap().as_mut() {
        if pending.release == version {
            pending.notes = release_notes.clone();
        }
    }
    write_screen(
        handle,
        &application_update_screen(version, release_notes.as_ref()),
    );
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

/// Shows ownership verification when a partially installed application update must finish.
fn show_required_update_signin(handle: &tauri::AppHandle, notes: Option<&ReleaseNotes>) {
    write_screen(handle, &required_update_signin_screen(notes));
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

/// Shows a failed operation with a primary retry and an optional secondary action.
fn show_retryable_failure(
    handle: &tauri::AppHandle,
    kicker: &str,
    status: &str,
    retry_choice: &str,
    secondary_action: Option<(&str, &str)>,
) {
    write_screen(
        handle,
        &retryable_failure_screen(kicker, status, retry_choice, secondary_action),
    );
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
            "window.rebSetStatus&&window.rebSetStatus({});",
            js_string(label)
        ),
    );
}

fn update_progress(handle: &tauri::AppHandle, percent: u64, label: &str) {
    window_eval(
        handle,
        &format!(
            "window.rebSetProgress&&window.rebSetProgress({percent},{});",
            js_string(label)
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

/// Returns one layer-specific directory for verified resumable downloads.
fn download_cache_dir(layer: &str) -> io::Result<PathBuf> {
    Ok(install_dir()?.join(".download-cache").join(layer))
}

/// Removes one committed layer's download cache without invalidating the completed update.
fn clear_download_cache(layer: &str) {
    match download_cache_dir(layer) {
        Ok(directory) => clear_committed_download_cache(&directory),
        Err(error) => log_line(&format!(
            "[launcher] unable to locate committed {layer} download cache: {error}"
        )),
    }
}

/// Removes expendable cached downloads after their update layer has committed.
fn clear_committed_download_cache(directory: &Path) {
    if let Err(error) = clear_cache(directory) {
        log_line(&format!(
            "[launcher] unable to clear committed download cache {}: {error}",
            directory.display()
        ));
    }
}

/// Restores every update layer that may have been interrupted during its commit phase.
fn recover_interrupted_updates() -> io::Result<usize> {
    let install_root = install_dir()?;
    let content = install_root.join("Content");
    let mut recovered = usize::from(recover_interrupted_apply(&install_root)?);
    recovered += usize::from(recover_interrupted_apply(&content)?);

    let packs = content.join("Packs");
    let entries = match fs::read_dir(packs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(recovered),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            recovered += usize::from(recover_interrupted_apply(&entry.path())?);
        }
    }
    Ok(recovered)
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
    let entry = format_log_entry(message, chrono::Utc::now());
    println!("{entry}");
    if let Ok(base) = install_dir() {
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("launcher.log"))
        {
            let _ = writeln!(file, "{entry}");
        }
    }
}

fn format_log_entry(message: &str, timestamp: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "{} {message}",
        timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    )
}

fn install_content(
    handle: &tauri::AppHandle,
    url: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = install_dir()?;
    let archive_path = base.join("content.zip");
    let content_dir = base.join("Content");
    let extraction_dir = base.join(CONTENT_INSTALL_STAGING_DIRECTORY);
    let version = requested_content_version().ok_or("the installed game version is unavailable")?;
    let content_base = content_base().ok_or("the content channel is not configured")?;
    let token = SESSION.lock().unwrap().clone();
    let expected: Manifest = fetch_json(
        &format!("{content_base}dist/manifest-{version}.json"),
        token.as_deref(),
    )?;
    validate_base_content_release_manifest(&expected, &version)?;

    log_line("[launcher] downloading Content…");
    let gib = (1u64 << 30) as f64;
    let mib = (1u64 << 20) as f64;
    let mut downloaded: u64 = 0;
    let mut last_report: u64 = 0;
    let mut last_time = Instant::now();
    let mut last_bytes: u64 = 0;
    let mut progress_initialized = false;
    let result = download_to_path(
        &DownloadSpec {
            url,
            authorization: None,
            destination: &archive_path,
            expected_size: None,
            expected_sha256: None,
        },
        |current| {
            if !progress_initialized {
                progress_initialized = true;
                downloaded = current;
                last_report = current;
                last_bytes = current;
                last_time = Instant::now();
                let total = known_download_size(&archive_path).unwrap_or(0);
                let percent = current.saturating_mul(100).checked_div(total).unwrap_or(0);
                if current > 0 {
                    update_progress(
                        handle,
                        percent,
                        &format!("Resuming Content… {:.2} GB", current as f64 / gib),
                    );
                } else {
                    update_progress(handle, 0, "Downloading Content… 0 KB");
                }
                return;
            }
            if current < downloaded {
                downloaded = current;
                last_report = current;
                last_bytes = current;
                last_time = Instant::now();
                let total = known_download_size(&archive_path).unwrap_or(0);
                let percent = current.saturating_mul(100).checked_div(total).unwrap_or(0);
                update_progress(
                    handle,
                    percent,
                    &format!(
                        "Restarting Content download… {:.2} GB",
                        current as f64 / gib
                    ),
                );
                return;
            }
            downloaded = current;
            if downloaded.saturating_sub(last_report) < 20 * (1 << 20)
                && last_time.elapsed() < Duration::from_secs(1)
            {
                return;
            }
            last_report = downloaded;
            let now = Instant::now();
            let elapsed = now.duration_since(last_time).as_secs_f64();
            let speed = if elapsed > 0.0 {
                downloaded.saturating_sub(last_bytes) as f64 / mib / elapsed
            } else {
                0.0
            };
            last_time = now;
            last_bytes = downloaded;
            let total = known_download_size(&archive_path).unwrap_or(0);
            let pct = downloaded
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0);
            let amount = if total > 0 {
                format!("{} / {}", human_bytes(downloaded), human_bytes(total))
            } else {
                human_bytes(downloaded)
            };
            update_progress(
                handle,
                pct,
                &format!("Downloading Content… {amount} — {speed:.1} MB/s"),
            );
        },
        |attempt, delay, error| {
            log_line(&format!(
                "[launcher] Content download interrupted ({error}); retrying attempt {attempt} in {}s.",
                delay.as_secs()
            ));
            update_status(
                handle,
                &format!(
                    "Connection interrupted. Resuming in {} seconds…",
                    delay.as_secs()
                ),
            );
        },
    );
    if let Err(error) = result {
        if error.status() == Some(404) {
            return Err(Box::new(ContentUnavailable));
        }
        return Err(Box::new(error));
    }
    update_progress(handle, 100, "Extracting Content…");

    remove_path_if_present(&extraction_dir)?;
    fs::create_dir_all(&extraction_dir)?;
    let archive_file = fs::File::open(&archive_path)?;
    let mut archive = match zip::ZipArchive::new(archive_file) {
        Ok(archive) => archive,
        Err(error) => {
            invalidate_content_archive(&archive_path);
            return Err(error.into());
        }
    };
    if let Err(error) = archive.extract(&extraction_dir) {
        drop(archive);
        invalidate_content_archive(&archive_path);
        return Err(error.into());
    }
    drop(archive);

    let target = match snapshot_base_content_directory(&extraction_dir, &version, true) {
        Ok(target) => target,
        Err(error) => {
            invalidate_content_archive(&archive_path);
            return Err(error.into());
        }
    };
    if let Err(error) = validate_base_content_manifest(&target) {
        invalidate_content_archive(&archive_path);
        return Err(error.into());
    }

    if !manifest_files_match(&target, &expected) {
        invalidate_content_archive(&archive_path);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "downloaded Content archive does not match its published manifest",
        )
        .into());
    }

    let local = snapshot_base_content_directory(&content_dir, "existing", false)?;
    let plan = diff(Some(&local), &target);
    let source = DirectoryBlobs {
        root: extraction_dir.clone(),
    };
    let manifest = serde_json::to_vec(&expected)?;
    let verified = rebellion2_update_core::sha256_hex(&manifest);
    apply_verified_with_metadata(
        &plan,
        &content_dir,
        &source,
        &[
            (CONTENT_MANIFEST_FILE, &manifest),
            (CONTENT_VERSION_FILE, version.as_bytes()),
            (CONTENT_VERIFICATION_FILE, verified.as_bytes()),
        ],
        &expected,
    )?;

    if let Err(error) = remove_path_if_present(&extraction_dir) {
        log_line(&format!(
            "[launcher] unable to remove completed Content staging directory: {error}"
        ));
    }
    let _ = fs::remove_file(&archive_path);
    if let Err(error) = clear_download_metadata(&archive_path) {
        log_line(&format!(
            "[launcher] unable to clear completed Content download metadata: {error}"
        ));
    }
    Ok(content_dir)
}

/// Removes a completed Content archive that failed validation so Retry downloads it again.
fn invalidate_content_archive(archive_path: &Path) {
    if let Err(error) = remove_path_if_present(archive_path) {
        log_line(&format!(
            "[launcher] unable to remove invalid Content archive: {error}"
        ));
    }
    if let Err(error) = clear_download_metadata(archive_path) {
        log_line(&format!(
            "[launcher] unable to clear invalid Content download metadata: {error}"
        ));
    }
}

/// Compares manifest file inventories without relying on source ordering or version labels.
fn manifest_files_match(left: &Manifest, right: &Manifest) -> bool {
    let mut left_files = left.files.clone();
    let mut right_files = right.files.clone();
    left_files.sort_by(|left, right| left.path.cmp(&right.path));
    right_files.sort_by(|left, right| left.path.cmp(&right.path));
    left_files == right_files
}

struct DirectoryBlobs {
    root: PathBuf,
}

impl BlobSource for DirectoryBlobs {
    fn fetch_to(&self, entry: &FileEntry, destination: &Path) -> io::Result<()> {
        fs::copy(self.root.join(&entry.path), destination)?;
        Ok(())
    }
}

/// Describes all base-content files in a directory while ignoring separately managed packs.
fn snapshot_base_content_directory(
    directory: &Path,
    version: &str,
    reject_unowned_files: bool,
) -> io::Result<Manifest> {
    let mut files = Vec::new();
    collect_base_content_files(directory, directory, reject_unowned_files, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Manifest {
        version: version.to_string(),
        files,
    })
}

fn collect_base_content_files(
    root: &Path,
    directory: &Path,
    reject_unowned_files: bool,
    files: &mut Vec<FileEntry>,
) -> io::Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !reject_unowned_files {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative != "Application"
                    && relative != "Packs"
                    && relative != "Packs/ClassicGalacticCivilWar"
                    && !relative.starts_with("Application/")
                    && !relative.starts_with("Packs/ClassicGalacticCivilWar/")
                {
                    continue;
                }
            }
            collect_base_content_files(root, &entry.path(), reject_unowned_files, files)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "content archive contains an unsupported filesystem entry",
            ));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        if !is_base_content_path(&relative) {
            if reject_unowned_files {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("content archive contains an unmanaged path: {relative}"),
                ));
            }
            continue;
        }
        let size = entry.metadata()?.len();
        files.push(FileEntry {
            path: relative,
            sha256: sha256_file(&entry.path())?,
            size,
        });
    }
    Ok(())
}

fn remove_path_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Includes obsolete native plugins in the player's rollback transaction without changing live files.
#[cfg(any(target_os = "windows", test))]
fn include_unlisted_windows_native_plugins(
    install_dir: &Path,
    manifest: &Manifest,
    plan: &mut rebellion2_update_core::Plan,
) -> io::Result<usize> {
    let plugin_directory = install_dir.join(WINDOWS_NATIVE_PLUGIN_DIRECTORY);
    let entries = match fs::read_dir(&plugin_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };

    let mut inventoried: std::collections::BTreeSet<String> = manifest
        .files
        .iter()
        .map(|entry| entry.path.to_ascii_lowercase())
        .chain(plan.removed.iter().map(|path| path.to_ascii_lowercase()))
        .collect();
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let relative_path = format!(
            "{WINDOWS_NATIVE_PLUGIN_DIRECTORY}/{}",
            entry.file_name().to_string_lossy()
        );
        if !inventoried.insert(relative_path.to_ascii_lowercase()) {
            continue;
        }
        plan.removed.push(relative_path);
        removed += 1;
    }
    Ok(removed)
}

/// Restores executable permissions after an interrupted Linux player update as well as before launch.
#[cfg(target_os = "linux")]
fn ensure_linux_player_executables(directory: &Path) -> io::Result<()> {
    ensure_executable(&directory.join(GAME_EXE))?;
    let crash_handler = directory.join(UNITY_CRASH_HANDLER_EXE);
    if crash_handler.is_file() {
        ensure_executable(&crash_handler)?;
    }
    Ok(())
}

fn launch_game() -> io::Result<bool> {
    recover_interrupted_updates()?;
    let base = install_dir()?;
    let content_version = read_installed_version(&base.join("Content"));
    if !content_matches_application(
        current_game_version().as_deref(),
        content_version.as_deref().unwrap_or_default(),
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the installed game and content versions do not match; finish the update before launching",
        ));
    }
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
    {
        ensure_linux_player_executables(&base)?;
        std::process::Command::new(&exe)
            .current_dir(&base)
            .spawn()?;
    }

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

    #[test]
    fn validate_game_manifest_with_update_state_paths_rejects_them() {
        for path in [
            ".game-version",
            ".game-manifest.json",
            ".content-packs/example/install.json",
            ".approved-content-update",
            ".rebellion2-install.lock",
        ] {
            let manifest = Manifest {
                version: "1.0.0".to_string(),
                files: vec![FileEntry {
                    path: path.to_string(),
                    sha256: sha256_hex(b"state"),
                    size: 5,
                }],
            };
            assert!(
                validate_game_manifest(&manifest, "1.0.0").is_err(),
                "accepted update state path {path}"
            );
        }
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn read_game_manifest_with_content_in_inventory_discards_the_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            files: vec![FileEntry {
                path: "Content/catalog.xml".to_string(),
                sha256: sha256_hex(b"content"),
                size: 7,
            }],
        };
        fs::write(
            directory.path().join(GAME_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(read_game_manifest(directory.path()).is_none());
    }

    #[test]
    fn write_file_atomically_replaces_complete_file_without_leaving_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.json");
        fs::write(&path, b"old").unwrap();

        write_file_atomically(path.clone(), b"new metadata").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new metadata");
        assert!(!path_with_suffix(&path, ".writing").exists());
    }

    #[test]
    fn staged_launcher_update_state_with_verified_files_is_ready() {
        let directory = tempfile::tempdir().unwrap();
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(
            directory.path().join(STAGED_LAUNCHER_FILE_NAME),
            LAUNCHER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            staged_launcher_update_state(
                directory.path(),
                &directory.path().join(LAUNCHER_FILE_NAME)
            )
            .unwrap(),
            StagedLauncherUpdateState::Ready
        );

        #[cfg(target_os = "windows")]
        {
            fs::write(
                directory.path().join(PENDING_APPLICATION_VERSION_FILE),
                b"stale",
            )
            .unwrap();
            assert_eq!(
                staged_launcher_update_state(
                    directory.path(),
                    &directory.path().join(LAUNCHER_FILE_NAME)
                )
                .unwrap(),
                StagedLauncherUpdateState::Invalid
            );
        }
    }

    #[test]
    fn staged_launcher_update_state_with_corrupt_executable_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(directory.path().join(STAGED_LAUNCHER_FILE_NAME), b"partial").unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            staged_launcher_update_state(
                directory.path(),
                &directory.path().join(LAUNCHER_FILE_NAME)
            )
            .unwrap(),
            StagedLauncherUpdateState::Invalid
        );
    }

    #[test]
    fn recover_completed_launcher_handoff_with_verified_files_promotes_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("installed-launcher");
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(&launcher, LAUNCHER_FILE_NAME.as_bytes()).unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(directory.path(), &launcher).unwrap(),
            CompletedLauncherHandoffState::Recovered
        );
        assert_eq!(
            fs::read(directory.path().join(LAUNCHER_MANIFEST_FILE)).unwrap(),
            serde_json::to_vec(&manifest).unwrap()
        );
        assert_eq!(
            fs::read(directory.path().join(LAUNCHER_VERSION_FILE)).unwrap(),
            b"1.2.3"
        );
        assert!(!directory
            .path()
            .join(PENDING_LAUNCHER_MANIFEST_FILE)
            .exists());
        assert!(!directory
            .path()
            .join(PENDING_LAUNCHER_VERSION_FILE)
            .exists());
    }

    #[test]
    fn recover_completed_launcher_handoff_after_manifest_promotion_finishes_version() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("installed-launcher");
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(&launcher, LAUNCHER_FILE_NAME.as_bytes()).unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(LAUNCHER_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(directory.path(), &launcher).unwrap(),
            CompletedLauncherHandoffState::Recovered
        );
        assert_eq!(
            fs::read(directory.path().join(LAUNCHER_VERSION_FILE)).unwrap(),
            b"1.2.3"
        );
        assert!(!directory
            .path()
            .join(PENDING_LAUNCHER_VERSION_FILE)
            .exists());
    }

    #[test]
    fn recover_completed_launcher_handoff_with_no_pending_metadata_does_nothing() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(
                directory.path(),
                &directory.path().join("missing-launcher")
            )
            .unwrap(),
            CompletedLauncherHandoffState::None
        );
    }

    #[test]
    fn recover_completed_launcher_handoff_with_corrupt_launcher_rejects_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("installed-launcher");
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(&launcher, b"corrupt").unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(directory.path(), &launcher).unwrap(),
            CompletedLauncherHandoffState::Invalid
        );
        assert!(directory
            .path()
            .join(PENDING_LAUNCHER_MANIFEST_FILE)
            .exists());
        assert!(directory
            .path()
            .join(PENDING_LAUNCHER_VERSION_FILE)
            .exists());
    }

    #[test]
    fn recover_completed_launcher_handoff_with_malformed_metadata_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_MANIFEST_FILE),
            b"not-json",
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(
                directory.path(),
                &directory.path().join("missing-launcher")
            )
            .unwrap(),
            CompletedLauncherHandoffState::Invalid
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn recover_completed_application_handoff_waits_for_matching_content() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("installed-launcher");
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(&launcher, LAUNCHER_FILE_NAME.as_bytes()).unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(directory.path(), &launcher).unwrap(),
            CompletedLauncherHandoffState::WaitingForContent
        );
        assert!(directory
            .path()
            .join(PENDING_APPLICATION_MANIFEST_FILE)
            .exists());
        assert!(directory
            .path()
            .join(PENDING_APPLICATION_VERSION_FILE)
            .exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn recover_completed_application_handoff_with_matching_content_promotes_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("installed-launcher");
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(&launcher, LAUNCHER_FILE_NAME.as_bytes()).unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();
        fs::create_dir(directory.path().join("Content")).unwrap();
        fs::write(
            directory.path().join("Content").join(CONTENT_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(directory.path(), &launcher).unwrap(),
            CompletedLauncherHandoffState::Recovered
        );
        assert!(directory.path().join(APPLICATION_MANIFEST_FILE).is_file());
        assert_eq!(
            fs::read(directory.path().join(APPLICATION_VERSION_FILE)).unwrap(),
            b"1.2.3"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn recover_completed_launcher_handoff_with_mixed_metadata_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(PENDING_LAUNCHER_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            recover_completed_launcher_handoff(
                directory.path(),
                &directory.path().join("missing-launcher")
            )
            .unwrap(),
            CompletedLauncherHandoffState::Invalid
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn staged_application_launcher_waits_for_matching_content_before_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        fs::write(
            directory.path().join(STAGED_LAUNCHER_FILE_NAME),
            LAUNCHER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(UPDATE_HELPER_FILE_NAME),
            UPDATE_HELPER_FILE_NAME.as_bytes(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(
            directory.path().join(PENDING_APPLICATION_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            staged_launcher_update_state(
                directory.path(),
                &directory.path().join(LAUNCHER_FILE_NAME)
            )
            .unwrap(),
            StagedLauncherUpdateState::WaitingForContent
        );

        fs::create_dir(directory.path().join("Content")).unwrap();
        fs::write(
            directory.path().join("Content").join(CONTENT_VERSION_FILE),
            b"1.2.3",
        )
        .unwrap();

        assert_eq!(
            staged_launcher_update_state(
                directory.path(),
                &directory.path().join(LAUNCHER_FILE_NAME)
            )
            .unwrap(),
            StagedLauncherUpdateState::Ready
        );
    }

    #[test]
    fn manifest_files_match_ignores_order_and_version_but_detects_different_files() {
        let left = Manifest {
            version: "first".to_string(),
            files: vec![entry_for_bytes("b", b"B"), entry_for_bytes("a", b"A")],
        };
        let mut right = Manifest {
            version: "second".to_string(),
            files: vec![entry_for_bytes("a", b"A"), entry_for_bytes("b", b"B")],
        };

        assert!(manifest_files_match(&left, &right));

        right.files[1] = entry_for_bytes("b", b"changed");
        assert!(!manifest_files_match(&left, &right));
    }

    #[test]
    fn include_unlisted_windows_native_plugins_preserves_live_files_until_commit() {
        let directory = tempfile::tempdir().unwrap();
        let plugin_directory = directory.path().join(WINDOWS_NATIVE_PLUGIN_DIRECTORY);
        fs::create_dir_all(&plugin_directory).unwrap();
        let retained = plugin_directory.join("retained.dll");
        let retired = plugin_directory.join("retired.dll");
        fs::write(&retained, b"retained").unwrap();
        fs::write(&retired, b"retired").unwrap();
        fs::create_dir(plugin_directory.join("nested")).unwrap();
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            files: vec![FileEntry {
                path: format!("{WINDOWS_NATIVE_PLUGIN_DIRECTORY}/RETAINED.dll"),
                sha256: "hash".to_string(),
                size: 8,
            }],
        };

        let mut plan = rebellion2_update_core::Plan::default();
        let removed =
            include_unlisted_windows_native_plugins(directory.path(), &manifest, &mut plan)
                .unwrap();

        assert_eq!(removed, 1);
        assert!(retired.exists());
        apply(&plan, directory.path(), &EmptyBlobs).unwrap();
        assert!(retained.exists());
        assert!(!retired.exists());
        assert!(plugin_directory.join("nested").exists());
    }

    #[test]
    fn include_unlisted_windows_native_plugins_without_plugin_directory_succeeds() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            files: Vec::new(),
        };

        let mut plan = rebellion2_update_core::Plan::default();
        let removed =
            include_unlisted_windows_native_plugins(directory.path(), &manifest, &mut plan)
                .unwrap();

        assert_eq!(removed, 0);
    }

    #[test]
    fn steam_retry_preserves_the_required_content_version() {
        let url: tauri::Url = steam_signin_url("https://gate.example/", Some("0.0.33+test"))
            .parse()
            .unwrap();
        assert_eq!(url.path(), "/start");
        assert_eq!(query(&url, "provider").as_deref(), Some("steam"));
        assert_eq!(query(&url, "v").as_deref(), Some("0.0.33+test"));
    }

    #[test]
    fn steam_retry_without_a_content_version_starts_steam_verification() {
        assert_eq!(
            steam_signin_url("https://gate.example/", None),
            "https://gate.example/start?provider=steam"
        );
    }

    #[test]
    fn unreadable_steam_library_offers_privacy_settings_and_fresh_verification() {
        let screen =
            verification_failure_screen(None, Some("steam_library_unavailable"), Some("public"));
        assert!(screen.contains("Your Steam game list is not visible to the launcher."));
        assert!(screen.contains("choice=steam-privacy"));
        assert!(screen.contains("choice=steam-retry"));
        assert!(screen.contains("My profile</b> and <b>Game details"));
        assert!(screen.contains(r#"choice=steam-retry">Retry</a>"#));
        assert!(screen.contains("Your profile is public, but your games list is hidden."));
        assert!(!screen.contains("your games are private"));
    }

    #[test]
    fn visible_library_without_eligible_game_keeps_a_distinct_explanation() {
        let screen =
            verification_failure_screen(None, Some("steam_game_not_found"), Some("public"));
        assert!(screen.contains("Neither eligible game appeared"));
        assert!(screen.contains("account that owns the game"));
        assert!(screen.contains("Your profile is public and your games list is visible."));
        assert!(!screen.contains("Your Steam game list is not visible to the launcher."));
    }

    #[test]
    fn other_verification_failures_keep_the_service_message_without_steam_privacy_actions() {
        let screen = verification_failure_screen(Some("Service unavailable <retry>"), None, None);
        assert!(screen.contains("Service unavailable &lt;retry&gt;"));
        assert!(screen.contains("choice=signin"));
        assert!(!screen.contains("choice=steam-privacy"));
    }

    #[test]
    fn unknown_verification_reason_does_not_infer_a_privacy_failure() {
        let screen = verification_failure_screen(None, Some("unknown"), None);
        assert!(screen.contains("Ownership could not be verified."));
        assert!(!screen.contains("choice=steam-privacy"));
    }

    #[test]
    fn non_public_profile_is_reported_separately_from_unavailable_game_list() {
        let screen = verification_failure_screen(
            None,
            Some("steam_library_unavailable"),
            Some("not_public"),
        );
        assert!(screen.contains("Your profile is not public, and your games list is hidden."));
    }

    #[test]
    fn unavailable_profile_visibility_does_not_guess_the_profile_setting() {
        let screen = verification_failure_screen(None, Some("steam_library_unavailable"), None);
        assert!(screen
            .contains("Your games list is hidden. Steam did not provide your profile visibility."));
    }

    #[test]
    fn content_pack_error_screen_separates_back_from_retry() {
        let screen = content_pack_error_screen("Installation failed", "Missing manifest");

        assert!(screen.contains("choice=back-to-ready\">Back</a>"));
        assert!(screen.contains("choice=content-packs\">Retry</a>"));
        assert!(!screen.contains("choice=content-packs\">Back</a>"));
        assert!(screen.contains("Missing manifest"));
    }

    #[test]
    fn retryable_failure_screen_separates_retry_from_continue() {
        let screen = retryable_failure_screen(
            "Update failed",
            "Any download progress was saved.",
            "game-update",
            Some(("Continue", "skip-game-update")),
        );

        assert!(screen.contains("choice=game-update\">Retry</a>"));
        assert!(screen.contains("choice=skip-game-update\">Continue</a>"));
        assert!(!screen.contains("choice=skip-game-update\">Retry</a>"));
    }

    #[test]
    fn presigned_download_only_requests_authorization_for_expired_link_statuses() {
        for status in [401, 403] {
            assert!(presigned_download_requires_authorization(
                &DownloadError::HttpStatus(status)
            ));
        }
        assert!(!presigned_download_requires_authorization(
            &DownloadError::HttpStatus(500)
        ));
        assert!(!presigned_download_requires_authorization(
            &DownloadError::InvalidData("truncated".to_string())
        ));
    }

    #[test]
    fn content_pack_download_error_identifies_missing_resource() {
        for resource in ["content-pack list", "content-pack manifest"] {
            let error = content_pack_download_error(resource, Box::new(ContentUnavailable));

            assert!(error.to_string().contains(resource));
            assert!(error.to_string().contains("HTTP 404"));
            assert!(!error.to_string().contains("launcher version"));
        }
    }

    #[test]
    fn content_pack_download_error_preserves_other_failure_details() {
        let error = content_pack_download_error(
            "content-pack list",
            Box::new(io::Error::new(io::ErrorKind::TimedOut, "request timed out")),
        );

        assert!(error.to_string().contains("content-pack list"));
        assert!(error.to_string().contains("request timed out"));
        assert!(!error.to_string().contains("HTTP 404"));
    }

    #[test]
    fn download_failure_log_records_resource_without_credentials_or_query() {
        let message = download_failure_log_message(
            "https://user:secret@packs.example.test/dist/catalogue.json?token=private#secret",
            "HTTP 404",
        );

        assert!(message.contains("https://packs.example.test/dist/catalogue.json"));
        assert!(message.contains("HTTP 404"));
        for secret in ["user:", "secret", "token=", "private"] {
            assert!(!message.contains(secret));
        }
    }

    #[test]
    fn log_entry_includes_utc_timestamp_and_message() {
        let timestamp = chrono::DateTime::from_timestamp(1_792_152_000, 123_000_000).unwrap();
        let entry = format_log_entry("[launcher] Session started.", timestamp);

        assert_eq!(
            entry,
            "2026-10-16T12:00:00.123Z [launcher] Session started."
        );
    }

    #[test]
    fn normalized_base_trims_and_appends_separator() {
        assert_eq!(
            normalized_base(Some("  https://packs.example.test  ")),
            Some("https://packs.example.test/".to_string())
        );
        assert_eq!(
            normalized_base(Some("https://packs.example.test/")),
            Some("https://packs.example.test/".to_string())
        );
    }

    #[test]
    fn normalized_base_rejects_missing_or_empty_values() {
        assert_eq!(normalized_base(None), None);
        assert_eq!(normalized_base(Some("   ")), None);
    }

    #[test]
    fn content_pack_manager_with_incompatible_game_keeps_only_remove_action() {
        let root = tempfile::tempdir().unwrap();
        let pack = content_pack_release("1.0.1");
        let manifest = application_manifest("1.0.1", &["pack.xml"]);
        content_packs::write_install(root.path(), &pack, &manifest).unwrap();
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![pack],
        };

        let screen = content_pack_manager_screen_for(&catalog, root.path(), Some("0.0.30"));

        assert!(screen.contains("Incompatible with game 0.0.30"));
        assert!(screen.contains("choice=content-pack-remove:sample-pack"));
        assert!(!screen.contains("choice=content-pack-install:sample-pack"));
    }

    #[test]
    fn content_pack_manager_with_newer_game_keeps_pack_compatible() {
        let root = tempfile::tempdir().unwrap();
        let pack = content_pack_release("1.0.1");
        let manifest = application_manifest("1.0.1", &["pack.xml"]);
        content_packs::write_install(root.path(), &pack, &manifest).unwrap();
        write_pack_definition(
            root.path(),
            "ClassicGalacticCivilWar",
            "classic-galactic-civil-war",
            "1.0.0",
        );
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![pack],
        };

        let screen = content_pack_manager_screen_for(&catalog, root.path(), Some("0.0.32"));

        assert!(screen.contains("Installed 1.0.1"));
        assert!(screen.contains("Requires Rebellion 2 &gt;=0.0.31"));
        assert!(!screen.contains("Incompatible with game"));
        assert!(screen.contains("choice=content-pack-remove:sample-pack"));
    }

    #[test]
    fn content_pack_manager_with_incompatible_content_dependency_keeps_only_remove_action() {
        let root = tempfile::tempdir().unwrap();
        let installed_pack = content_pack_release("1.0.0");
        let manifest = application_manifest("1.0.0", &["pack.xml"]);
        content_packs::write_install(root.path(), &installed_pack, &manifest).unwrap();
        write_pack_definition(
            root.path(),
            "ClassicGalacticCivilWar",
            "classic-galactic-civil-war",
            "0.9.0",
        );
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![content_pack_release("1.0.1")],
        };

        let screen = content_pack_manager_screen_for(&catalog, root.path(), Some("0.0.31"));

        assert!(screen.contains("Incompatible with classic-galactic-civil-war 0.9.0"));
        assert!(screen.contains("choice=content-pack-remove:sample-pack"));
        assert!(!screen.contains("choice=content-pack-install:sample-pack"));
    }

    #[test]
    fn pending_installed_content_pack_update_finds_newer_compatible_release() {
        let root = tempfile::tempdir().unwrap();
        let installed = content_pack_release("1.0.1");
        let manifest = application_manifest("1.0.1", &["pack.xml"]);
        content_packs::write_install(root.path(), &installed, &manifest).unwrap();
        write_pack_definition(
            root.path(),
            "ClassicGalacticCivilWar",
            "classic-galactic-civil-war",
            "1.0.0",
        );
        let mut current = content_pack_release("1.0.3");
        current.minimum_supported_version = Some("1.0.3".to_string());
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![current],
        };

        let (release, installed) =
            pending_installed_content_pack_update(&catalog, root.path(), Some("0.0.34"))
                .expect("a compatible update should be required");

        assert_eq!(release.version, "1.0.3");
        assert_eq!(installed.version, "1.0.1");
    }

    #[test]
    fn pending_installed_content_pack_update_current_release_returns_none() {
        let root = tempfile::tempdir().unwrap();
        let installed = content_pack_release("1.0.3");
        let manifest = application_manifest("1.0.3", &["pack.xml"]);
        content_packs::write_install(root.path(), &installed, &manifest).unwrap();
        write_pack_definition(
            root.path(),
            "ClassicGalacticCivilWar",
            "classic-galactic-civil-war",
            "1.0.0",
        );
        let mut current = content_pack_release("1.0.3");
        current.minimum_supported_version = Some("1.0.3".to_string());
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![current],
        };

        assert!(
            pending_installed_content_pack_update(&catalog, root.path(), Some("0.0.34")).is_none()
        );
    }

    #[test]
    fn pending_installed_content_pack_update_optional_release_returns_none() {
        let root = tempfile::tempdir().unwrap();
        let installed = content_pack_release("1.0.1");
        let manifest = application_manifest("1.0.1", &["pack.xml"]);
        content_packs::write_install(root.path(), &installed, &manifest).unwrap();
        write_pack_definition(
            root.path(),
            "ClassicGalacticCivilWar",
            "classic-galactic-civil-war",
            "1.0.0",
        );
        let catalog = ContentPackCatalog {
            schema_version: 2,
            packs: vec![content_pack_release("1.0.3")],
        };

        assert!(
            pending_installed_content_pack_update(&catalog, root.path(), Some("0.0.34")).is_none()
        );
    }

    #[test]
    fn required_content_pack_update_screen_omits_launch_bypass() {
        let release = content_pack_release("1.0.3");
        let installed = content_packs::InstalledContentPack {
            id: release.id.clone(),
            name: release.name.clone(),
            version: "1.0.1".to_string(),
            directory: release.directory.clone(),
        };

        let screen = required_content_pack_update_screen(&release, &installed);

        assert!(screen.contains("Content pack update required"));
        assert!(screen.contains("Update content pack"));
        assert!(!screen.contains("choice=play"));
    }

    #[test]
    fn installed_optional_base_version_uses_actual_pack_definition() {
        let root = tempfile::tempdir().unwrap();
        let mut base_pack = content_pack_release("2.0.0");
        base_pack.id = "foundation-pack".to_string();
        base_pack.directory = "FoundationPack".to_string();
        base_pack.depends.remove("classic-galactic-civil-war");
        let manifest = application_manifest("2.0.0", &["pack.xml"]);
        content_packs::write_install(root.path(), &base_pack, &manifest).unwrap();
        write_pack_definition(root.path(), "FoundationPack", "foundation-pack", "1.0.0");

        assert_eq!(
            installed_content_pack_version(root.path(), "foundation-pack").as_deref(),
            Some("1.0.0")
        );

        write_pack_definition(root.path(), "FoundationPack", "foundation-pack", "2.0.0");

        assert_eq!(
            installed_content_pack_version(root.path(), "foundation-pack").as_deref(),
            Some("2.0.0")
        );
    }

    fn entry_for_bytes(path: &str, bytes: &[u8]) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    fn application_manifest(version: &str, paths: &[&str]) -> Manifest {
        Manifest {
            version: version.to_string(),
            files: paths
                .iter()
                .map(|path| entry_for_bytes(path, path.as_bytes()))
                .collect(),
        }
    }

    fn content_pack_release(version: &str) -> ContentPackRelease {
        ContentPackRelease {
            id: "sample-pack".to_string(),
            name: "Sample Pack".to_string(),
            description: "Sample content.".to_string(),
            version: version.to_string(),
            directory: "SamplePack".to_string(),
            manifest: format!("dist/content-packs/sample-pack/manifest-{version}.json"),
            blobs: "content-pack-blobs/".to_string(),
            download_size: 100,
            minimum_supported_version: None,
            depends: std::collections::BTreeMap::from([
                (
                    GAME_DEPENDENCY_ID.to_string(),
                    content_packs::VersionRequirement::Single(">=0.0.31".to_string()),
                ),
                (
                    "classic-galactic-civil-war".to_string(),
                    content_packs::VersionRequirement::Single("1.0.0".to_string()),
                ),
            ]),
        }
    }

    fn write_pack_definition(root: &Path, directory: &str, id: &str, version: &str) {
        let pack_root = root.join("Content").join("Packs").join(directory);
        fs::create_dir_all(&pack_root).unwrap();
        fs::write(
            pack_root.join("pack.xml"),
            format!(
                "<ContentPack><ID>{id}</ID><Version>{version}</Version><Preloads><Preload><ID>main-menu</ID><Path>Preload/main-menu.xml</Path></Preload><Preload><ID>strategy</ID><Path>Preload/strategy.xml</Path></Preload></Preloads></ContentPack>"
            ),
        )
        .unwrap();
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
    fn ensure_executable_on_unix_sets_execute_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("program");

        fs::write(&path, b"program").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        ensure_executable(&path).unwrap();

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
    fn launcher_manifest_with_invalid_hash_returns_error() {
        let mut manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        manifest.files[0].sha256 = "../outside".to_string();

        assert!(validate_launcher_manifest(&manifest, "1.2.3").is_err());
    }

    #[test]
    fn game_manifest_with_player_files_returns_ok() {
        let manifest = application_manifest("0.0.26", &["Rebellion2.exe", "Data/shared.dat"]);

        assert!(validate_game_manifest(&manifest, "0.0.26").is_ok());
    }

    #[test]
    fn game_manifest_with_invalid_hash_returns_error() {
        let mut manifest = application_manifest("0.0.26", &["Rebellion2.exe"]);
        manifest.files[0].sha256 = "../outside".to_string();

        assert!(validate_game_manifest(&manifest, "0.0.26").is_err());
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
    fn game_manifest_with_download_cache_returns_error() {
        let manifest = application_manifest("0.0.26", &[".download-cache/game/blob"]);

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
    fn parse_release_notes_with_versioned_document_preserves_current_and_historical_sections() {
        let bytes = br#"{
            "version":"1.2.3",
            "sections":[{"title":"Fixes","items":["Fixed current issue."]}],
            "releases":[
                {"version":"1.2.2","sections":[{"title":"Changes","items":["Improved previous behavior."]}]},
                {"version":"1.2.3","sections":[{"title":"Fixes","items":["Fixed current issue."]}]}
            ]
        }"#;

        let notes = parse_release_notes(bytes, "1.2.3", &sha256_hex(bytes))
            .expect("versioned release notes should remain readable");

        assert_eq!(notes.sections[0].items, vec!["Fixed current issue."]);
        assert_eq!(notes.releases.len(), 2);
        assert_eq!(notes.releases[0].version, "1.2.2");
        assert_eq!(
            notes.releases[0].sections[0].items,
            vec!["Improved previous behavior."]
        );
        assert_eq!(notes.releases[1].version, notes.version);
        assert_eq!(notes.releases[1].sections[0].items, notes.sections[0].items);
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
    fn scaled_download_percent_without_transferred_bytes_stays_at_phase_start() {
        assert_eq!(scaled_download_percent(0, 100, 0, 95), 0);
        assert_eq!(scaled_download_percent(0, 100, 48, 95), 48);
        assert_eq!(scaled_download_percent(20, 0, 0, 95), 0);
    }

    #[test]
    fn scaled_download_percent_with_transferred_bytes_scales_and_clamps() {
        assert_eq!(scaled_download_percent(50, 100, 0, 95), 47);
        assert_eq!(scaled_download_percent(100, 100, 0, 95), 95);
        assert_eq!(scaled_download_percent(200, 100, 0, 95), 95);
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

        let compact_screen: String = screen.split_whitespace().collect();
        assert!(compact_screen.contains(".card{position:relative;width:100%;height:100%"));
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
    fn validate_application_manifest_with_invalid_hash_returns_error() {
        let mut manifest =
            application_manifest("1.2.3", &[LAUNCHER_FILE_NAME, UPDATE_HELPER_FILE_NAME]);
        manifest.files[0].sha256 = "../outside".to_string();

        assert!(validate_application_manifest(&manifest, "1.2.3").is_err());
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
    fn base_content_manifest_rejects_optional_pack_files() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                "catalog.xml",
                "Application/Schemas/content-pack.xsd",
                "Packs/ClassicGalacticCivilWar/pack.xml",
                "Packs/OptionalPack/pack.xml",
            ],
        );

        assert!(validate_base_content_manifest(&manifest).is_err());
    }

    #[test]
    fn base_content_release_manifest_rejects_different_version() {
        let manifest = application_manifest(
            "1.2.3",
            &[
                "catalog.xml",
                "Application/Schemas/content-pack.xsd",
                "Packs/ClassicGalacticCivilWar/pack.xml",
            ],
        );

        assert!(validate_base_content_release_manifest(&manifest, "1.2.4").is_err());
    }

    #[test]
    fn base_content_repair_transaction_preserves_optional_pack_directory() {
        let root = tempfile::tempdir().unwrap();
        let content = root.path().join("Content");
        let replacement = root.path().join("Replacement");
        fs::create_dir_all(content.join("Application")).unwrap();
        fs::create_dir_all(content.join("Packs/ClassicGalacticCivilWar")).unwrap();
        fs::create_dir_all(content.join("Packs/OptionalPack")).unwrap();
        fs::write(content.join("catalog.xml"), b"old catalog").unwrap();
        fs::write(content.join("Application/removed.xml"), b"removed").unwrap();
        fs::write(
            content.join("Packs/ClassicGalacticCivilWar/pack.xml"),
            b"old base pack",
        )
        .unwrap();
        fs::write(content.join("Packs/OptionalPack/pack.xml"), b"optional").unwrap();
        fs::create_dir_all(replacement.join("Application")).unwrap();
        fs::create_dir_all(replacement.join("Packs/ClassicGalacticCivilWar")).unwrap();
        fs::write(replacement.join("catalog.xml"), b"new catalog").unwrap();
        fs::write(replacement.join("Application/added.xml"), b"added").unwrap();
        fs::write(
            replacement.join("Packs/ClassicGalacticCivilWar/pack.xml"),
            b"new base pack",
        )
        .unwrap();

        let local = snapshot_base_content_directory(&content, "old", false).unwrap();
        let target = snapshot_base_content_directory(&replacement, "new", true).unwrap();
        apply(
            &diff(Some(&local), &target),
            &content,
            &DirectoryBlobs { root: replacement },
        )
        .unwrap();

        assert_eq!(
            fs::read(content.join("catalog.xml")).unwrap(),
            b"new catalog"
        );
        assert!(!content.join("Application/removed.xml").exists());
        assert_eq!(
            fs::read(content.join("Application/added.xml")).unwrap(),
            b"added"
        );
        assert_eq!(
            fs::read(content.join("Packs/ClassicGalacticCivilWar/pack.xml")).unwrap(),
            b"new base pack"
        );
        assert_eq!(
            fs::read(content.join("Packs/OptionalPack/pack.xml")).unwrap(),
            b"optional"
        );
    }

    #[test]
    fn base_content_snapshot_ignores_optional_pack_files() {
        let root = tempfile::tempdir().unwrap();
        let content = root.path().join("Content");
        fs::create_dir_all(content.join("Application")).unwrap();
        fs::create_dir_all(content.join("Packs/OptionalPack")).unwrap();
        fs::write(content.join("Application/base.xml"), b"base").unwrap();
        fs::write(content.join("Packs/OptionalPack/pack.xml"), b"optional").unwrap();

        let manifest = snapshot_base_content_directory(&content, "1.0.0", false).unwrap();

        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "Application/base.xml");
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
