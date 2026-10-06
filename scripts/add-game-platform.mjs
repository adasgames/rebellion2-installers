import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const SOURCE_FORMATS = new Set(["application", "game"]);
const PLATFORMS = new Set(["windows", "macos", "linux"]);

/** Adds one signed platform layer to a coherent game release pointer. */
export function addGamePlatform(
  source,
  sourceFormat,
  version,
  platform,
  manifest,
  blobs,
  signature,
) {
  if (!isObject(source)) throw new Error("Release pointer must be an object.");
  if (!SOURCE_FORMATS.has(sourceFormat)) {
    throw new Error(`Unsupported source format: ${sourceFormat}`);
  }
  if (!PLATFORMS.has(platform)) {
    throw new Error(`Unsupported game platform: ${platform}`);
  }
  for (const [name, value] of Object.entries({ version, manifest, blobs, signature })) {
    if (typeof value !== "string" || value.length === 0) {
      throw new Error(`${name} must be a non-empty string.`);
    }
  }
  if (source.version !== version || !isObject(source.content) || source.content.version !== version) {
    throw new Error("Release and content versions must match the requested version.");
  }

  let platforms;
  if (sourceFormat === "game") {
    if (!isObject(source.platforms)) {
      throw new Error("Game release platforms must be an object.");
    }
    platforms = { ...source.platforms };
  } else {
    platforms = {};
  }
  if (platforms[platform] != null) {
    throw new Error(`Game release already supports ${platform}.`);
  }
  platforms[platform] = { manifest, blobs, signature };

  return {
    version,
    platforms,
    content: source.content,
  };
}

/** Returns whether a value is a non-array object. */
function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function main() {
  const [sourceFormat, inputPath, outputPath, version, platform, manifest, blobs, signature] =
    process.argv.slice(2);
  if (!signature) {
    throw new Error(
      "Usage: node scripts/add-game-platform.mjs <application|game> <input.json> <output.json> <version> <windows|macos|linux> <manifest> <blobs> <signature>",
    );
  }
  const source = JSON.parse(await readFile(inputPath, "utf8"));
  const pointer = addGamePlatform(
    source,
    sourceFormat,
    version,
    platform,
    manifest,
    blobs,
    signature,
  );
  await writeFile(outputPath, `${JSON.stringify(pointer)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
