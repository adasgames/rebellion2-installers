use crate::versions::{compare_versions, version_gt};
use rebellion2_update_core::sha256_hex;
use serde::Deserialize;

/// A versioned release-notes document published beside the update manifests.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReleaseNotesPointer {
    pub(crate) path: String,
    pub(crate) sha256: String,
}

/// Release notes shown on the update confirmation screen.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReleaseNotes {
    pub(crate) version: String,
    pub(crate) sections: Vec<ReleaseNoteSection>,
    #[serde(default, rename = "breaksSaves")]
    pub(crate) breaks_saves: bool,
    #[serde(default)]
    pub(crate) releases: Vec<VersionedReleaseNotes>,
}

/// A titled group of release-note items.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReleaseNoteSection {
    pub(crate) title: String,
    pub(crate) items: Vec<String>,
}

/// Release-note sections published for one historical version.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VersionedReleaseNotes {
    pub(crate) version: String,
    pub(crate) sections: Vec<ReleaseNoteSection>,
    #[serde(default, rename = "breaksSaves")]
    pub(crate) breaks_saves: bool,
}
/// Parses release notes only when their version and digest match the release pointer.
pub(crate) fn parse_release_notes(
    bytes: &[u8],
    version: &str,
    sha256: &str,
) -> Option<ReleaseNotes> {
    if sha256_hex(bytes) != sha256 {
        return None;
    }

    let notes = serde_json::from_slice::<ReleaseNotes>(bytes).ok()?;
    if notes.version != version
        || (notes.sections.is_empty()
            && !notes.breaks_saves
            && !notes.releases.iter().any(|release| release.breaks_saves))
        || !release_note_sections_are_valid(&notes.sections)
        || notes.releases.iter().any(|release| {
            release.version.trim().is_empty()
                || version_gt(&release.version, &notes.version)
                || (release.sections.is_empty() && !release.breaks_saves)
                || !release_note_sections_are_valid(&release.sections)
        })
    {
        return None;
    }

    Some(notes)
}

/// Returns whether every release-note section has a title and at least one item.
fn release_note_sections_are_valid(sections: &[ReleaseNoteSection]) -> bool {
    sections
        .iter()
        .all(|section| !section.title.trim().is_empty() && !section.items.is_empty())
}

/// Merges every published release newer than the installed content into one view.
pub(crate) fn aggregate_release_notes(
    mut notes: ReleaseNotes,
    installed_version: Option<&str>,
) -> ReleaseNotes {
    let Some(installed_version) = installed_version else {
        return notes;
    };
    notes.breaks_saves &= version_gt(&notes.version, installed_version);
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
    let breaks_saves = notes.breaks_saves || selected.iter().any(|release| release.breaks_saves);

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
        breaks_saves,
        releases: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(document: serde_json::Value) -> ReleaseNotes {
        let bytes = serde_json::to_vec(&document).unwrap();
        parse_release_notes(&bytes, "0.3.1", &sha256_hex(&bytes)).unwrap()
    }

    #[test]
    fn parse_release_notes_defaults_missing_save_flag_to_false() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1",
            "sections": [{"title": "Fixes", "items": ["Fixed repairs."]}],
            "releases": [{
                "version": "0.3.0",
                "sections": [{"title": "Changes", "items": ["Improved repairs."]}]
            }]
        }));

        assert!(!notes.breaks_saves);
        assert!(!notes.releases[0].breaks_saves);
    }

    #[test]
    fn parse_release_notes_accepts_a_save_warning_without_sections() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1", "breaksSaves": true, "sections": []
        }));

        assert!(notes.breaks_saves);
    }

    #[test]
    fn parse_release_notes_retains_historical_warning_when_current_release_has_no_sections() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1", "sections": [],
            "releases": [{"version": "0.3.0", "breaksSaves": true, "sections": []}]
        }));

        assert!(aggregate_release_notes(notes, Some("0.2.9")).breaks_saves);
    }

    #[test]
    fn parse_release_notes_rejects_a_non_boolean_save_flag() {
        let bytes = br#"{"version":"0.3.1","breaksSaves":"true","sections":[]}"#;

        assert!(parse_release_notes(bytes, "0.3.1", &sha256_hex(bytes)).is_none());
    }

    #[test]
    fn aggregate_release_notes_warns_when_skipping_a_save_breaking_release() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1",
            "sections": [{"title": "Fixes", "items": ["Fixed repairs."]}],
            "releases": [
                {"version": "0.3.0", "breaksSaves": true, "sections": []},
                {"version": "0.3.1", "sections": [{"title": "Fixes", "items": ["Fixed repairs."]}]}
            ]
        }));

        assert!(aggregate_release_notes(notes.clone(), Some("0.2.9")).breaks_saves);
        assert!(!aggregate_release_notes(notes.clone(), Some("0.3.0")).breaks_saves);
        assert!(!aggregate_release_notes(notes, Some("0.3.1")).breaks_saves);
    }

    #[test]
    fn aggregate_release_notes_keeps_the_current_warning_when_history_omits_current_release() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1", "breaksSaves": true, "sections": [],
            "releases": [{"version": "0.3.0", "sections": [{"title": "Fixes", "items": ["Fixed repairs."]}]}]
        }));

        assert!(aggregate_release_notes(notes, Some("0.2.9")).breaks_saves);
    }

    #[test]
    fn aggregate_release_notes_does_not_warn_about_current_version_repairs_or_rollbacks() {
        let notes = parse(serde_json::json!({
            "version": "0.3.1", "breaksSaves": true, "sections": []
        }));

        assert!(aggregate_release_notes(notes.clone(), Some("0.3.0")).breaks_saves);
        assert!(!aggregate_release_notes(notes.clone(), Some("0.3.1")).breaks_saves);
        assert!(!aggregate_release_notes(notes, Some("0.3.2")).breaks_saves);
    }
}
