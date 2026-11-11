import { pathToFileURL } from "node:url";

import {
  latestPlatformRelease,
  publishPlatformDownload,
  verifyPlatformDownload,
} from "./publish-platform-download.mjs";

const platform = {
  id: "macos",
  label: "macOS",
  aliasTag: "latest-macos-download",
  assetName: "Rebellion2-macOS.zip",
  versionedAssetName: () => "Rebellion2-macOS.zip",
  script: "publish-macos-download.mjs",
};

/** Selects the highest published stable game version with a macOS download. */
export function latestMacRelease(releases) {
  return latestPlatformRelease(releases, platform);
}

/** Rejects changed or incomplete bytes before replacing the public alias asset. */
export function verifyMacDownload(bytes, asset) {
  verifyPlatformDownload(bytes, asset, platform);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await publishPlatformDownload(platform);
}
