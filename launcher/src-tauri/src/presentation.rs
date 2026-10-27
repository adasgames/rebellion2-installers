use crate::content_packs::{self, ContentPackRelease};
use crate::downloads::DownloadError;
use crate::release_notes::ReleaseNotes;
use crate::ACT;
use std::{error::Error, io};

/// Describes local storage, access, verification, and connection failures separately.
pub(crate) fn operation_failure_status(error: &(dyn Error + 'static)) -> &'static str {
    let mut current = Some(error);
    while let Some(cause) = current {
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            match error.kind() {
                io::ErrorKind::StorageFull => return "There isn't enough free disk space. Free some space, then retry.",
                io::ErrorKind::PermissionDenied => return "A file is in use or cannot be changed. Close the game and other launcher windows, then retry.",
                io::ErrorKind::InvalidData => return "The downloaded files could not be verified. Retry to download them again.",
                io::ErrorKind::TimedOut | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected => return "The connection was interrupted. Download progress was saved; retry to continue.",
                _ => {}
            }
            if let Some(inner) = error.get_ref() {
                current = Some(inner);
                continue;
            }
        }
        if let Some(error) = cause.downcast_ref::<DownloadError>() {
            match error {
                DownloadError::Connection(_) => return "The connection was interrupted. Download progress was saved; retry to continue.",
                DownloadError::HttpStatus(_) => return "The download server is busy or unavailable. Download progress was saved; retry shortly.",
                DownloadError::InvalidData(_) => return "The downloaded files could not be verified. Retry to download them again.",
                DownloadError::Io(_) => {}
            }
        }
        if let Some(error) = cause.downcast_ref::<ureq::Error>() {
            return match error {
                ureq::Error::Transport(_) => "The connection was interrupted. Download progress was saved; retry to continue.",
                ureq::Error::Status(_, _) => "The download server is busy or unavailable. Download progress was saved; retry shortly.",
            };
        }
        current = cause.source();
    }
    "The update could not finish. Download progress was saved. See launcher.log for details."
}

pub(crate) fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// One shared full-window layout: kicker + REBELLION II wordmark top-aligned, an
/// optional spinner, a status line, a progress bar, and buttons pinned toward the
/// bottom.
pub(crate) fn render(kicker: &str, spinner: bool, status: &str, buttons: &str) -> String {
    render_with_content(kicker, spinner, status, "", buttons)
}

/// Renders the shared launcher layout with optional content above its actions.
pub(crate) fn render_with_content(
    kicker: &str,
    spinner: bool,
    status: &str,
    content: &str,
    buttons: &str,
) -> String {
    render_card(kicker, spinner, status, "sub", content, buttons)
}

pub(crate) fn render_card(
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
        include_str!("../../ui/screen.html"),
        styles = include_str!("../../ui/launcher.css"),
        kicker = kicker,
        status = status,
        spin = spin,
        content = content,
        card_class = card_class,
        status_class = status_class,
        buttons = buttons,
    )
}

pub(crate) fn button(label: &str, choice: &str) -> String {
    format!(
        "<a class=\"b primary\" href=\"{}?choice={}\">{}</a>",
        ACT, choice, label
    )
}

pub(crate) fn secondary_button(label: &str, choice: &str) -> String {
    format!(
        "<a class=\"b secondary\" href=\"{}?choice={}\">{}</a>",
        ACT, choice, label
    )
}

pub(crate) fn disabled_button(label: &str) -> String {
    format!("<span class=\"b disabled\">{}</span>", label)
}

/// Builds the content-update confirmation screen with optional release notes.
pub(crate) fn update_available_screen(
    detail: &str,
    notes: Option<&ReleaseNotes>,
    buttons: &str,
) -> String {
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content("Update available", false, detail, &release_notes, buttons)
}

/// Renders structured release notes for the update confirmation screen.
pub(crate) fn render_release_notes(notes: &ReleaseNotes) -> String {
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

/// Maps explicit Steam visibility outcomes to wording that does not infer private settings.
pub(crate) fn steam_verification_status(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some("steam_library_unavailable") => {
            Some("Your Steam game list is not visible to the launcher.")
        }
        Some("steam_game_not_found") => {
            Some("Neither eligible game appeared in the list Steam shared.")
        }
        _ => None,
    }
}

/// Builds a provider-specific recovery screen from the ownership service's explicit outcome.
pub(crate) fn verification_failure_screen(
    message: Option<&str>,
    reason: Option<&str>,
    profile_visibility: Option<&str>,
) -> String {
    if let Some(status) = steam_verification_status(reason) {
        return steam_verification_help_screen(status, reason, profile_visibility);
    }
    let status = message
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("Ownership could not be verified. Please try again.");
    render(
        "Not verified",
        false,
        status,
        &button("Try Again", "signin"),
    )
}

/// Presents Steam privacy instructions while keeping the pending installation or update intact.
pub(crate) fn steam_verification_help_screen(
    status: &str,
    reason: Option<&str>,
    profile_visibility: Option<&str>,
) -> String {
    let visibility = match (reason, profile_visibility) {
        (Some("steam_library_unavailable"), Some("public")) => {
            "Your profile is public, but your games list is hidden."
        }
        (Some("steam_library_unavailable"), Some("not_public")) => {
            "Your profile is not public, and your games list is hidden."
        }
        (Some("steam_library_unavailable"), _) => {
            "Your games list is hidden. Steam did not provide your profile visibility."
        }
        (Some("steam_game_not_found"), Some("public")) => {
            "Your profile is public and your games list is visible."
        }
        (Some("steam_game_not_found"), Some("not_public")) => {
            "Your profile is not public, but your games list is visible."
        }
        _ => "Your games list is visible. Steam did not provide your profile visibility.",
    };
    let content = format!(
        r#"<section class="release-changes" style="margin:16px 0;color:#cbd2e3;font-size:12px;line-height:1.6">
        <p>{visibility}</p>
        <ol style="padding-left:20px">
          <li><a href="{ACT}?choice=steam-privacy" style="color:#ffcf4d;text-underline-offset:3px">Open Steam privacy settings</a>.</li>
          <li>Set <b>My profile</b> and <b>Game details</b> to <b>Public</b>.</li>
          <li>Use the account that owns the game. If the game itself is marked private, make it public for verification.</li>
          <li>Return here and choose <b>Retry</b>.</li>
        </ol>
        </section>"#
    );
    let buttons = button("Retry", "steam-retry") + &secondary_button("Back", "signin");
    render_card(
        "Steam verification",
        false,
        status,
        "sub failure-error",
        &content,
        &buttons,
    )
}

/// Renders the required optional-content update without offering a launch bypass.
pub(crate) fn required_content_pack_update_screen(
    release: &ContentPackRelease,
    installed: &content_packs::InstalledContentPack,
) -> String {
    let buttons = format!(
        "{}{}",
        button(
            "Update content pack",
            &format!("content-pack-install:{}", release.id)
        ),
        secondary_button("Manage content packs", "content-packs")
    );
    render(
        "Content pack update required",
        false,
        &format!(
            "{} must be updated from {} to {} before launching this game version.",
            release.name, installed.version, release.version
        ),
        &buttons,
    )
}

pub(crate) fn up_to_date_screen(version: &str) -> String {
    render(
        "Up to date",
        false,
        &format!("Version {version} is up to date."),
        &ready_buttons("Launch game"),
    )
}

pub(crate) fn ready_buttons(play_label: &str) -> String {
    let mut buttons = button(play_label, "play");
    buttons.push_str(&secondary_button("Content packs", "content-packs"));
    buttons
}

pub(crate) fn content_pack_error_screen(title: &str, error: &str) -> String {
    let buttons = format!(
        "{}{}",
        button("Retry", "content-packs"),
        secondary_button("Back", "back-to-ready")
    );
    render(
        title,
        false,
        &format!("{error} See launcher.log for details."),
        &buttons,
    )
}

/// Builds the ready screen after staging a launcher update and checking the game.
pub(crate) fn launcher_staged_ready_screen(version: &str) -> String {
    render(
        "Ready to play",
        false,
        &format!(
            "Game version {version} is up to date. The launcher update will apply next time you start the launcher."
        ),
        &ready_buttons("Play game"),
    )
}

pub(crate) fn update_check_failed_screen(installed_version: Option<&str>, error: &str) -> String {
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

pub(crate) fn update_check_failure_reason(error: &str) -> &str {
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

/// Builds the confirmation screen for an independent launcher update.
pub(crate) fn launcher_update_screen(version: &str, notes: Option<&ReleaseNotes>) -> String {
    let buttons = format!(
        "<a class=\"b primary\" href=\"{a}?choice=launcher-update\">Update and restart</a>\
         <a class=\"b secondary\" href=\"{a}?choice=skip-launcher-update\">Not now</a>",
        a = ACT,
    );
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Launcher update available",
        false,
        &format!("Launcher version {version} is available. The launcher will restart and continue where you left off."),
        &release_notes,
        &buttons,
    )
}

/// Builds the confirmation screen for a game-player and content release.
pub(crate) fn game_update_screen(
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

/// Builds the confirmation screen shown before an application update downloads.
pub(crate) fn application_update_screen(version: &str, notes: Option<&ReleaseNotes>) -> String {
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

/// Builds the ownership prompt shown before protected update content downloads.
pub(crate) fn update_signin_screen(notes: Option<&ReleaseNotes>) -> String {
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

/// Builds the ownership prompt used after application files have been staged.
pub(crate) fn required_update_signin_screen(notes: Option<&ReleaseNotes>) -> String {
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

/// Builds a failed-operation screen with a primary retry and optional secondary action.
pub(crate) fn retryable_failure_screen(
    kicker: &str,
    status: &str,
    retry_choice: &str,
    secondary_action: Option<(&str, &str)>,
) -> String {
    let mut actions = button("Retry", retry_choice);
    if let Some((label, choice)) = secondary_action {
        actions.push_str(&secondary_button(label, choice));
    }
    render(kicker, false, status, &actions)
}

/// Builds the update progress screen with optional cumulative release notes.
pub(crate) fn progress_screen(notes: Option<&ReleaseNotes>) -> String {
    let release_notes = notes.map(render_release_notes).unwrap_or_default();
    render_with_content(
        "Updating",
        false,
        "Preparing update\u{2026}",
        &release_notes,
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_failure_status_with_full_disk_explains_storage_failure() {
        let error = DownloadError::Io(io::Error::from(io::ErrorKind::StorageFull));
        assert!(operation_failure_status(&error).contains("free disk space"));
    }

    #[test]
    fn operation_failure_status_with_wrapped_file_error_does_not_blame_connection() {
        let error = io::Error::other(DownloadError::Io(io::Error::from(
            io::ErrorKind::PermissionDenied,
        )));
        let status = operation_failure_status(&error);
        assert!(status.contains("file"));
        assert!(!status.contains("connection"));
    }

    #[test]
    fn operation_failure_status_with_wrapped_network_failure_explains_resuming() {
        let error =
            DownloadError::Connection(io::Error::from(io::ErrorKind::TimedOut)).into_io_error();
        assert!(operation_failure_status(&error).contains("retry to continue"));
    }

    #[test]
    fn operation_failure_status_with_wrapped_server_failure_identifies_server() {
        let error = DownloadError::HttpStatus(503).into_io_error();
        assert!(operation_failure_status(&error).contains("server"));
    }

    #[test]
    fn operation_failure_status_with_invalid_data_explains_verification() {
        let error = DownloadError::InvalidData("bad hash".to_string()).into_io_error();
        assert!(operation_failure_status(&error).contains("verified"));
    }

    #[test]
    fn operation_failure_status_with_unknown_error_points_to_log() {
        let error = io::Error::other("other local failure");
        assert!(operation_failure_status(&error).contains("launcher.log"));
    }
}
