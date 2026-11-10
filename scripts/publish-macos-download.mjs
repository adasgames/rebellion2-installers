import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { compareSemanticVersions } from "./semver.mjs";

const ALIAS_TAG = "latest-macos-download";
const ASSET_NAME = "Rebellion2-macOS.zip";

/** Selects the highest published stable game version with a macOS download. */
export function latestMacRelease(releases) {
  let latest = null;
  for (const release of releases) {
    if (
      release.draft ||
      release.prerelease ||
      !/^v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/u.test(release.tag_name)
    ) {
      continue;
    }
    const asset = release.assets.find(
      (candidate) =>
        candidate.name === ASSET_NAME &&
        candidate.state === "uploaded" &&
        candidate.size > 0,
    );
    if (!asset) continue;
    const version = release.tag_name.slice(1);
    if (!latest || compareSemanticVersions(version, latest.version) > 0) {
      latest = { release, asset, version };
    }
  }
  if (!latest) {
    throw new Error("No published stable macOS game download was found.");
  }
  return latest;
}

/** Rejects changed or incomplete bytes before replacing the public alias asset. */
export function verifyMacDownload(bytes, asset) {
  const digest = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  if (bytes.length !== asset.size || digest !== asset.digest) {
    throw new Error(
      "The macOS download does not match its published size and SHA-256 digest.",
    );
  }
}

/** Runs GitHub CLI without passing release metadata through a shell. */
function github(...arguments_) {
  return execFileSync("gh", arguments_, {
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
}

/** Refreshes direct downloads independently of the legacy macOS updater feed. */
async function main() {
  const [repository, mode = "--dry-run", ...extra] = process.argv.slice(2);
  if (
    !/^[\w.-]+\/[\w.-]+$/u.test(repository ?? "") ||
    !["--dry-run", "--publish"].includes(mode) ||
    extra.length
  ) {
    throw new Error(
      "Usage: node scripts/publish-macos-download.mjs <owner/repo> [--dry-run|--publish]",
    );
  }
  const releases = github(
    "api",
    "--paginate",
    `repos/${repository}/releases?per_page=100`,
    "--jq",
    ".[] | @json",
  )
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const { release, asset, version } = latestMacRelease(releases);
  console.log(`Latest macOS download: ${release.tag_name}/${ASSET_NAME}`);
  if (mode !== "--publish") return;
  if (!/^sha256:[0-9a-f]{64}$/u.test(asset.digest ?? "")) {
    throw new Error("The published macOS download has no SHA-256 digest.");
  }

  const alias = releases.find((candidate) => candidate.tag_name === ALIAS_TAG);
  if (alias && (alias.draft || !alias.prerelease)) {
    throw new Error(
      "Refusing to replace a release that is not a published download alias.",
    );
  }
  const title = `Latest Rebellion 2 macOS download (v${version})`;
  if (
    alias?.name === title &&
    alias.assets.some(
      (candidate) =>
        candidate.name === ASSET_NAME &&
        candidate.state === "uploaded" &&
        candidate.digest === asset.digest &&
        candidate.size === asset.size,
    )
  ) {
    console.log("The macOS download alias is already current.");
    return;
  }

  const directory = await mkdtemp(join(tmpdir(), "reb2-macos-download-"));
  try {
    github(
      "release",
      "download",
      release.tag_name,
      "--repo",
      repository,
      "--pattern",
      ASSET_NAME,
      "--dir",
      directory,
    );
    const download = join(directory, ASSET_NAME);
    verifyMacDownload(await readFile(download), asset);
    const notes = join(directory, "notes.md");
    await writeFile(
      notes,
      `Download for the newest published macOS game release: [Rebellion 2 ${version}](https://github.com/${repository}/releases/tag/${release.tag_name}).\n`,
    );
    const metadata = [
      "--repo",
      repository,
      "--title",
      title,
      "--notes-file",
      notes,
      "--prerelease",
      "--latest=false",
    ];
    if (alias) {
      github(
        "release",
        "upload",
        ALIAS_TAG,
        download,
        "--repo",
        repository,
        "--clobber",
      );
      github("release", "edit", ALIAS_TAG, ...metadata);
    } else {
      github(
        "release",
        "create",
        ALIAS_TAG,
        download,
        "--target",
        release.target_commitish,
        ...metadata,
      );
    }
    const published = JSON.parse(
      github("api", `repos/${repository}/releases/tags/${ALIAS_TAG}`),
    );
    const publishedAsset = published.assets.find(
      (candidate) => candidate.name === ASSET_NAME,
    );
    if (
      published.draft ||
      !published.prerelease ||
      publishedAsset?.state !== "uploaded" ||
      publishedAsset?.digest !== asset.digest ||
      publishedAsset?.size !== asset.size
    ) {
      throw new Error(
        "The published macOS download alias does not match the selected release.",
      );
    }
    console.log(
      `Published https://github.com/${repository}/releases/download/${ALIAS_TAG}/${ASSET_NAME}`,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
