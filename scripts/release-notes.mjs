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

async function main() {
  const [inputPath, outputPath, version] = process.argv.slice(2);
  if (!inputPath || !outputPath || !version) {
    throw new Error(
      "Usage: node scripts/release-notes.mjs <input.md> <output.json> <version>",
    );
  }

  const markdown = await readFile(inputPath, "utf8");
  const notes = parseReleaseNotes(markdown, version);
  await writeFile(outputPath, `${JSON.stringify(notes, null, 2)}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
