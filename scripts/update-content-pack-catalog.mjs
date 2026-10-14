import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const slug = /^[a-z0-9]+(?:-[a-z0-9]+)*$/u;
const directory = /^[A-Za-z0-9][A-Za-z0-9._-]*$/u;

/** Adds or replaces one independently published release in the pack catalog. */
export function updateContentPackCatalog(catalog, release, manifest) {
  if (catalog.schemaVersion !== 1 || !Array.isArray(catalog.packs)) {
    throw new Error("Content-pack catalog must use schema version 1.");
  }
  validateRelease(release);
  if (manifest.version !== release.version || !Array.isArray(manifest.files)) {
    throw new Error("Content-pack manifest does not match the release version.");
  }
  if (!manifest.files.some((entry) => entry.path === "pack.xml")) {
    throw new Error("Content-pack manifest must contain pack.xml.");
  }
  const downloadSize = manifest.files.reduce((total, entry) => total + entry.size, 0);
  const next = {
    ...release,
    downloadSize,
  };
  const packs = catalog.packs.filter((pack) => pack.id !== release.id);
  const directoryOwner = packs.find(
    (pack) => pack.directory.toLowerCase() === release.directory.toLowerCase(),
  );
  if (directoryOwner) {
    throw new Error(
      `Install directory ${release.directory} already belongs to ${directoryOwner.id}.`,
    );
  }
  packs.push(next);
  packs.sort((left, right) => left.name.localeCompare(right.name));
  return { schemaVersion: 1, packs };
}

function validateRelease(release) {
  if (!slug.test(release.id)) throw new Error("Content-pack id must be a lowercase slug.");
  if (!release.name?.trim()) throw new Error("Content-pack name is required.");
  if (!release.version?.trim()) throw new Error("Content-pack version is required.");
  if (
    !directory.test(release.directory) ||
    release.directory === "." ||
    release.directory === ".."
  ) {
    throw new Error("Content-pack directory is unsafe.");
  }
  if (
    !isChannelPath(release.manifest) ||
    !isChannelPath(release.blobs) ||
    !release.blobs.endsWith("/")
  ) {
    throw new Error("Content-pack channel path is unsafe.");
  }
  if (release.basePackId && !slug.test(release.basePackId)) {
    throw new Error("Base content-pack id must be a lowercase slug.");
  }
}

function isChannelPath(value) {
  const normalized = value?.replace(/\/+$/u, "");
  return Boolean(normalized) &&
    !normalized.startsWith("/") &&
    !normalized.includes("\\") &&
    !normalized.includes(":") &&
    normalized.split("/").every((part) => part && part !== "." && part !== "..");
}

async function main() {
  const [catalogPath, releasePath, manifestPath, outputPath] = process.argv.slice(2);
  if (!catalogPath || !releasePath || !manifestPath || !outputPath) {
    throw new Error(
      "Usage: node scripts/update-content-pack-catalog.mjs <catalog.json> <release.json> <manifest.json> <output.json>",
    );
  }
  let catalog = { schemaVersion: 1, packs: [] };
  try {
    catalog = JSON.parse(await readFile(catalogPath, "utf8"));
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const release = JSON.parse(await readFile(releasePath, "utf8"));
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  const updated = updateContentPackCatalog(catalog, release, manifest);
  await writeFile(outputPath, `${JSON.stringify(updated, null, 2)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
