use std::cmp::Ordering as VersionOrdering;

/// One validated semantic version split into precedence-bearing identifiers.
pub(crate) struct SemanticVersion<'a> {
    core: [&'a str; 3],
    prerelease: Vec<&'a str>,
}

/// True if semantic version `a` is newer than `b`.
pub(crate) fn version_gt(a: &str, b: &str) -> bool {
    compare_versions(a, b) == VersionOrdering::Greater
}

/// Compares semantic versions without truncating arbitrarily large numeric identifiers.
pub(crate) fn compare_versions(a: &str, b: &str) -> VersionOrdering {
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
pub(crate) fn parse_semantic_version(version: &str) -> Option<SemanticVersion<'_>> {
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
