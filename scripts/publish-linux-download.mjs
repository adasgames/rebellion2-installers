import { pathToFileURL } from "node:url";

import {
  latestPlatformRelease,
  publishPlatformDownload,
  verifyPlatformDownload,
} from "./publish-platform-download.mjs";

const platform = {
  id: "linux",
  label: "Linux",
  aliasTag: "latest-linux-download",
  assetName: "Rebellion2-Linux-x86_64.AppImage",
  versionedAssetName: (version) => `Rebellion2-${version}-x86_64.AppImage`,
  script: "publish-linux-download.mjs",
};

/** Selects the highest published stable game version with a Linux AppImage. */
export function latestLinuxRelease(releases) {
  return latestPlatformRelease(releases, platform);
}

/** Rejects changed or incomplete bytes before replacing the public alias asset. */
export function verifyLinuxDownload(bytes, asset) {
  verifyPlatformDownload(bytes, asset, platform);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await publishPlatformDownload(platform);
}
