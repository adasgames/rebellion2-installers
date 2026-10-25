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
pub(crate) fn aggregate_release_notes(
    notes: ReleaseNotes,
    installed_version: Option<&str>,
) -> ReleaseNotes {
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
