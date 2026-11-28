import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { latestPlatformRelease } from "./publish-platform-download.mjs";

const PLATFORMS = {
  windows: "Windows.zip",
  macos: "macOS.zip",
  linux: "Linux.tar.gz",
};

/** Selects the current platform's launcher, falling back to its latest published bundle. */
export function selectLauncherRelease(channel, releases, platform) {
  if (!Object.hasOwn(PLATFORMS, platform)) {
    throw new Error(`Unsupported launcher platform: ${platform}`);
  }
  const layer = channel?.platforms?.[platform];
  if (layer != null) {
    if (
      typeof channel?.version !== "string" ||
      channel.version.length === 0 ||
      typeof layer.manifest !== "string" ||
      layer.manifest.length === 0
    ) {
      throw new Error(`Invalid ${platform} launcher channel.`);
    }
    return { version: channel.version, manifest: layer.manifest };
  }
  const { version } = latestPlatformRelease(releases, {
    label: platform,
    tagPrefix: "launcher-v",
    releaseKind: "launcher",
    versionedAssetName: () => `Rebellion2-launcher-${PLATFORMS[platform]}`,
  });
  return {
    version,
    manifest: `dist/launcher-manifest-${platform}-${version}.json`,
  };
}

/** Writes one platform's launcher selection for the installer workflows. */
async function main() {
  const [channelPath, releasesPath, platform, outputPath] =
    process.argv.slice(2);
  if (!outputPath) {
    throw new Error(
      "Usage: node scripts/select-launcher-release.mjs <channel.json> <releases.json> <platform> <output.json>",
    );
  }
  const channel = JSON.parse(await readFile(channelPath, "utf8"));
  const releases = JSON.parse(await readFile(releasesPath, "utf8")).flat();
  await writeFile(
    outputPath,
    `${JSON.stringify(selectLauncherRelease(channel, releases, platform))}\n`,
  );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await main();
}
