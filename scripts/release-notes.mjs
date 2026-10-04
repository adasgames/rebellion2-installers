import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

/**
 * Converts the supported release-description Markdown into launcher JSON.
 * Only second-level headings and the bullet items beneath them are published.
 */
export function parseReleaseNotes(markdown, version) {
  const sections = [];
  let currentSection = null;

  for (const rawLine of markdown.split(/\r?\n/u)) {
    const line = rawLine.trim();
    const heading = /^##\s+(.+)$/u.exec(line);
    if (heading) {
      currentSection = { title: heading[1].trim(), items: [] };
      sections.push(currentSection);
      continue;
    }

    const item = /^(?:\*|-)\s+(.+)$/u.exec(line);
    if (item && currentSection) {
      currentSection.items.push(item[1].trim());
    }
  }

  return {
    version,
    sections: sections.filter(
      (section) => section.title.length > 0 && section.items.length > 0,
    ),
  };
}

/**
 * Builds one backward-compatible release-notes document with versioned history.
 * Older launchers continue reading `sections`; newer launchers can select and
 * merge the entries in `releases` from the installed version onward.
 */
export function buildReleaseNotes(
  markdown,
  version,
  publishedReleases = [],
  tagPrefix = "v",
) {
  const current = parseReleaseNotes(markdown, version);
  const escapedPrefix = tagPrefix.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
  const tagPattern = new RegExp(
    `^${escapedPrefix}(\\d+\\.\\d+\\.\\d+)$`,
    "u",
  );
  const releases = publishedReleases
    .filter(
      (release) =>
        !release.draft &&
        !release.prerelease &&
        tagPattern.test(release.tag_name ?? ""),
    )
    .map((release) => {
      const match = tagPattern.exec(release.tag_name);
      return parseReleaseNotes(release.body ?? "", match[1]);
    })
    .filter(
      (release) =>
        release.sections.length > 0 && compareVersions(release.version, version) < 0,
    );

  if (current.sections.length > 0) {
    releases.push({
      version: current.version,
      sections: current.sections,
    });
  }
  releases.sort((left, right) => compareVersions(left.version, right.version));

  return { ...current, releases };
}

/** Compares numeric dotted release versions. */
function compareVersions(left, right) {
  const parse = (value) => value.split(".").map((part) => Number.parseInt(part, 10));
  const leftParts = parse(left);
  const rightParts = parse(right);
  const length = Math.max(leftParts.length, rightParts.length);
  for (let index = 0; index < length; index += 1) {
    const difference = (leftParts[index] ?? 0) - (rightParts[index] ?? 0);
    if (difference !== 0) {
      return difference;
    }
  }
  return 0;
}

async function main() {
  const [inputPath, outputPath, version, releaseHistoryPath, tagPrefix] =
    process.argv.slice(2);
  if (!inputPath || !outputPath || !version) {
    throw new Error(
      "Usage: node scripts/release-notes.mjs <input.md> <output.json> <version> [release-history.json] [tag-prefix]",
    );
  }

  const markdown = await readFile(inputPath, "utf8");
  const publishedReleases = releaseHistoryPath
    ? JSON.parse(await readFile(releaseHistoryPath, "utf8"))
    : [];
  const notes = buildReleaseNotes(
    markdown,
    version,
    publishedReleases,
    tagPrefix ?? "v",
  );
  await writeFile(outputPath, `${JSON.stringify(notes, null, 2)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
