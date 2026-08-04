// Rebellion II launcher (Tauri)
//
// Opens the content gate in a native webview. The user signs in with Steam or GOG
// inside this window, so we can watch the navigation and capture the result
// ourselves — no copy-paste, no local server. On success the gate hands back a
// short-lived presigned URL; we download Content/ next to the launcher (showing
// progress in the window title), then start the game.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Instant;
use std::{fs, io, thread};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const GATE: &str = "https://rebellion2-content-gate.pages.dev/";

/// The content version this launcher was built for, baked in by the installer build
/// (REB2_CONTENT_VERSION). The gate resolves it to content-<version>.zip.
const CONTENT_VERSION: Option<&str> = option_env!("REB2_CONTENT_VERSION");

/// File written after a successful Content installation. Its value identifies
/// the pack expected by this launcher build.
const CONTENT_VERSION_FILE: &str = ".content-version";

/// Name of the game executable the launcher starts once Content is in place.
#[cfg(target_os = "windows")]
const GAME_EXE: &str = "Rebellion2.exe";
#[cfg(target_os = "linux")]
const GAME_EXE: &str = "Rebellion2";
#[cfg(target_os = "macos")]
const GAME_EXE: &str = "Rebellion2.app";

/// Where users download a fresh installer when their version's Content is gone.
const RELEASES_URL: &str = "https://github.com/davidadas/rebellion2-installers/releases/latest";

/// Signals that this launcher version's Content is no longer on R2 (e.g. pruned by
/// retention), so the download 404s and the user must update rather than retry.
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

/// Builds the gate landing URL, carrying the baked content version so the gate presigns
/// the matching pack. Falls back to the bare gate when no version was baked in.
fn gate_landing() -> String {
    match CONTENT_VERSION {
        Some(v) if !v.is_empty() => format!("{GATE}?v={}", urlencoding::encode(v)),
        _ => GATE.to_string(),
    }
}

fn main() {
    if should_launch_installed_game() {
        match launch_game() {
            Ok(true) => return,
            Ok(false) => log_line(
                "[launcher] installed Content is current, but the game executable is missing.",
            ),
            Err(err) => log_line(&format!(
                "[launcher] failed to start the installed game: {err}"
            )),
        }
    }

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(gate_landing().parse().unwrap()),
            )
            .title("Rebellion II")
            .inner_size(520.0, 700.0)
            .resizable(false)
            .on_navigation(move |url| on_nav(&handle, url))
            .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Rebellion II launcher");
}

/// Returns whether this launcher can skip verification and start the installed game.
/// `--repair` deliberately bypasses this check and opens the ownership gate.
fn should_launch_installed_game() -> bool {
    if std::env::args().any(|argument| argument == "--repair") {
        return false;
    }

    let Some(expected_version) = CONTENT_VERSION.filter(|version| !version.is_empty()) else {
        return false;
    };
    let Ok(base) = install_dir() else {
        return false;
    };
    let content_dir = base.join("Content");
    if !content_dir.join("catalog.xml").is_file() {
        return false;
    }

    fs::read_to_string(content_dir.join(CONTENT_VERSION_FILE))
        .map(|installed_version| installed_version.trim() == expected_version)
        .unwrap_or(false)
}

/// Intercepts every navigation in the launcher webview.
///
/// * `handle` - handle used to drive the webview and app when we react to a URL.
/// * `url` - the URL the webview is about to load.
/// * returns `true` to allow the navigation, `false` to cancel it.
fn on_nav(handle: &tauri::AppHandle, url: &tauri::Url) -> bool {
    let target = url.as_str();

    // GOG lands the ?code= on GOG's own page — grab it, then have the gate exchange it.
    if target.starts_with("https://embed.gog.com/on_login_success") {
        if let Some(code) = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
        {
            if let Some(window) = handle.get_webview_window("main") {
                let mut callback =
                    format!("{GATE}callback/gog?code={}", urlencoding::encode(&code));
                if let Some(v) = CONTENT_VERSION {
                    if !v.is_empty() {
                        callback.push_str(&format!("&v={}", urlencoding::encode(v)));
                    }
                }
                if let Ok(parsed) = callback.parse() {
                    let _ = window.navigate(parsed);
                }
            }
            return false; // don't actually load GOG's page
        }
    }

    // Terminal result page from the gate: /done?ok=1&url=<presigned>
    if target.starts_with(&format!("{GATE}done")) {
        let ok = url
            .query_pairs()
            .any(|(key, value)| key == "ok" && value == "1");
        let download_url = url
            .query_pairs()
            .find(|(key, _)| key == "url")
            .map(|(_, value)| value.into_owned());
        on_result(handle, ok, download_url);
    }

    true
}

/// Handles the verification outcome once the gate reaches its terminal page.
///
/// * `handle` - handle used to drive the window and app afterwards.
/// * `ok` - whether the signed-in account was verified as owning the game.
/// * `download_url` - the short-lived presigned Content URL, present only when verified.
fn on_result(handle: &tauri::AppHandle, ok: bool, download_url: Option<String>) {
    match (ok, download_url) {
        (true, Some(url)) => {
            log_line("[launcher] VERIFIED — received download URL; starting Content install.");
            start_install(handle.clone(), url);
        }
        (true, None) => log_line("[launcher] VERIFIED but the gate returned no download URL."),
        (false, _) => log_line("[launcher] DENIED — this account does not own the title."),
    }
}

/// Downloads and extracts Content on a background thread, then launches the game.
///
/// * `handle` - handle used for progress titles and to close the launcher on success.
/// * `url` - the presigned Content URL to download.
fn start_install(handle: tauri::AppHandle, url: String) {
    show_progress_ui(&handle);
    thread::spawn(move || match install_content(&handle, &url) {
        Ok(content_dir) => {
            log_line(&format!(
                "[launcher] Content installed to {}",
                content_dir.display()
            ));
            set_title(&handle, "Rebellion II — Starting game…");
            update_progress(&handle, 100, "Starting game…");
            match launch_game() {
                Ok(true) => {
                    log_line("[launcher] game started — closing launcher.");
                    handle.exit(0);
                }
                Ok(false) => log_line("[launcher] Content ready; game executable not present."),
                Err(err) => log_line(&format!("[launcher] failed to start the game: {err}")),
            }
        }
        Err(err) if err.downcast_ref::<ContentUnavailable>().is_some() => {
            log_line("[launcher] Content for this version is unavailable — prompting update.");
            set_title(&handle, "Rebellion II — Update required");
            show_update_required_ui(&handle);
        }
        Err(err) => {
            log_line(&format!("[launcher] failed to install Content: {err}"));
            set_title(&handle, "Rebellion II — Download failed");
            update_progress(&handle, 0, "Download failed — see launcher.log");
        }
    });
}

/// Replaces the webview contents with a native-looking install progress screen.
///
/// * `handle` - handle to the app whose main window is repainted.
fn show_progress_ui(handle: &tauri::AppHandle) {
    let html = r##"<!doctype html><html><head><meta charset="utf-8"><style>*{box-sizing:border-box}html,body{margin:0;height:100%}body{display:flex;flex-direction:column;align-items:center;justify-content:center;background:radial-gradient(900px 600px at 70% -10%,#1a2547 0%,transparent 55%),linear-gradient(180deg,#0b1226,#05070f);color:#e8ecf6;font-family:"Segoe UI",system-ui,sans-serif}.k{letter-spacing:.42em;font-size:10px;color:#ffcf4d;text-transform:uppercase;margin-bottom:14px;opacity:.9}.t{font-size:30px;font-weight:800;letter-spacing:.18em}.t span{color:#ffcf4d}.s{color:#8a93ad;font-size:13px;margin:24px 0 10px}.bar{width:74%;max-width:360px;height:10px;background:rgba(255,255,255,.08);border-radius:6px;overflow:hidden}.f{height:100%;width:0%;background:linear-gradient(90deg,#ffcf4d,#ffa83d);transition:width .3s ease}.p{color:#8a93ad;font-size:12px;margin-top:10px}</style></head><body><div class="k">Installing</div><div class="t">REBELLION <span>II</span></div><div class="s" id="s">Preparing…</div><div class="bar"><div class="f" id="f"></div></div><div class="p" id="p"></div><script>window.rebSetProgress=function(p,l){var f=document.getElementById("f");if(f)f.style.width=p+"%";var pe=document.getElementById("p");if(pe)pe.textContent=p+"%";if(l){var s=document.getElementById("s");if(s)s.textContent=l;}};</script></body></html>"##;
    let js = format!("document.open();document.write('{html}');document.close();");
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.eval(&js);
    }
}

/// Static HTML for the update-required screen; {URL} is replaced with the releases link.
const UPDATE_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8"><style>*{box-sizing:border-box}html,body{margin:0;height:100%}body{display:flex;flex-direction:column;align-items:center;justify-content:center;background:radial-gradient(900px 600px at 70% -10%,#1a2547 0%,transparent 55%),linear-gradient(180deg,#0b1226,#05070f);color:#e8ecf6;font-family:"Segoe UI",system-ui,sans-serif;text-align:center;padding:0 32px}.k{letter-spacing:.42em;font-size:10px;color:#ffcf4d;text-transform:uppercase;margin-bottom:14px}.t{font-size:30px;font-weight:800;letter-spacing:.18em}.t span{color:#ffcf4d}.s{color:#c7cede;font-size:14px;margin:22px 0 10px;line-height:1.5}a{color:#ffcf4d;font-size:13px;word-break:break-all}.ic{margin-bottom:16px;line-height:0}</style></head><body><div class="ic"><svg width="54" height="54" viewBox="0 0 24 24"><path d="M12 2 22 21H2Z" fill="#ffcf4d"/><path d="M11 8h2v6h-2zM11 16h2v2h-2z" fill="#0b1226"/></svg></div><div class="k">Update required</div><div class="t">REBELLION <span>II</span></div><div class="s">This version is no longer supported.<br>Please download and install the latest build:</div><a href="{URL}">{URL}</a></body></html>"##;

/// Replaces the webview with an "update required" screen, shown when this version's
/// Content is no longer available on R2 and the user must install a newer build.
///
/// * `handle` - handle to the app whose main window is repainted.
fn show_update_required_ui(handle: &tauri::AppHandle) {
    let html = UPDATE_HTML.replace("{URL}", RELEASES_URL);
    let js = format!("document.open();document.write('{html}');document.close();");
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.eval(&js);
    }
}

/// Updates the install progress screen's bar and status text.
///
/// * `handle` - handle to the app.
/// * `percent` - completion percentage (0-100).
/// * `label` - status line to show (ASCII/no quotes; supplied internally).
fn update_progress(handle: &tauri::AppHandle, percent: u64, label: &str) {
    let js = format!("window.rebSetProgress&&window.rebSetProgress({percent},\"{label}\");");
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.eval(&js);
    }
}

/// Sets the launcher window title (used to surface download progress).
///
/// * `handle` - handle to the app.
/// * `title` - the new window title.
fn set_title(handle: &tauri::AppHandle, title: &str) {
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.set_title(title);
    }
}

/// Resolves the directory the launcher lives in — where Content and the game exe sit.
///
/// * returns the launcher's install directory, or an IO error if it can't be resolved.
fn install_dir() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "launcher has no parent directory"))
}

/// Appends a line to launcher.log next to the exe (and to stdout) so the install
/// flow is observable even in release builds, which have no console.
///
/// * `message` - the line to record.
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

/// Downloads the presigned Content archive (with progress) and extracts it next to
/// the launcher.
///
/// * `handle` - handle used to report download progress in the window title.
/// * `url` - the short-lived presigned R2 URL for content.zip.
/// * returns the path to the extracted `Content` directory, or an error.
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
        // A pruned/missing content version presigns to a 404 (R2 NoSuchKey) — treat that
        // as "this build is too old" rather than a generic download failure.
        Err(ureq::Error::Status(404, _)) => return Err(Box::new(ContentUnavailable)),
        Err(other) => return Err(Box::new(other)),
    };
    let total: u64 = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = fs::File::create(&archive_path)?;
    let mut buffer = vec![0u8; 1 << 20]; // 1 MiB
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
            let pct = if total > 0 {
                downloaded * 100 / total
            } else {
                0
            };
            let amount = if total > 0 {
                format!("{gb:.2} / {total_gb:.2} GB")
            } else {
                format!("{gb:.2} GB")
            };
            set_title(handle, &format!("Rebellion II — {pct}% ({speed:.0} MB/s)"));
            update_progress(
                handle,
                pct,
                &format!("Downloading Content… {amount} — {speed:.1} MB/s"),
            );
        }
    }
    file.sync_all()?;
    drop(file);
    log_line(&format!(
        "[launcher] downloaded {downloaded} bytes; extracting…"
    ));
    set_title(handle, "Rebellion II — Extracting…");
    update_progress(handle, 100, "Extracting Content…");

    if content_dir.exists() {
        fs::remove_dir_all(&content_dir)?;
    }
    fs::create_dir_all(&content_dir)?;
    let archive_file = fs::File::open(&archive_path)?;
    let mut archive = zip::ZipArchive::new(archive_file)?;
    archive.extract(&content_dir)?;
    if let Some(version) = CONTENT_VERSION.filter(|version| !version.is_empty()) {
        fs::write(content_dir.join(CONTENT_VERSION_FILE), version)?;
    }
    log_line(&format!("[launcher] extracted {} entries.", archive.len()));

    let _ = fs::remove_file(&archive_path);
    Ok(content_dir)
}

/// Starts the game executable if it is installed alongside the launcher.
///
/// * returns `Ok(true)` if the game was started, `Ok(false)` if the executable is
///   not present yet, or an error if starting it failed.
fn launch_game() -> io::Result<bool> {
    let base = install_dir()?;
    let exe = base.join(GAME_EXE);
    if !exe.exists() {
        return Ok(false);
    }

    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&exe).spawn()?;
    #[cfg(not(target_os = "macos"))]
    std::process::Command::new(&exe)
        .current_dir(&base)
        .spawn()?;

    Ok(true)
}
