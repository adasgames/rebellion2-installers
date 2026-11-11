import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { compareSemanticVersions } from "./semver.mjs";

/** Selects the highest published stable game version with the requested platform download. */
export function latestPlatformRelease(releases, platform) {
  let latest = null;
  for (const release of releases) {
    if (
      release.draft ||
      release.prerelease ||
      !/^v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/u.test(
        release.tag_name,
      )
    ) {
      continue;
    }
    const version = release.tag_name.slice(1);
    const asset = release.assets.find(
      (candidate) =>
        candidate.name === platform.versionedAssetName(version) &&
        candidate.state === "uploaded" &&
        candidate.size > 0,
    );
    if (!asset) continue;
    if (!latest || compareSemanticVersions(version, latest.version) > 0) {
      latest = { release, asset, version };
    }
  }
  if (!latest) {
    throw new Error(
      `No published stable ${platform.label} game download was found.`,
    );
  }
  return latest;
}

/** Rejects changed or incomplete bytes before replacing the public alias asset. */
export function verifyPlatformDownload(bytes, asset, platform) {
  const digest = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  if (bytes.length !== asset.size || digest !== asset.digest) {
    throw new Error(
      `The ${platform.label} download does not match its published size and SHA-256 digest.`,
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

/** Refreshes a platform download alias from its newest published stable game asset. */
export async function publishPlatformDownload(
  platform,
  arguments_ = process.argv.slice(2),
) {
  const [repository, mode = "--dry-run", ...extra] = arguments_;
  if (
    !/^[\w.-]+\/[\w.-]+$/u.test(repository ?? "") ||
    !["--dry-run", "--publish"].includes(mode) ||
    extra.length
  ) {
    throw new Error(
      `Usage: node scripts/${platform.script} <owner/repo> [--dry-run|--publish]`,
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
  const { release, asset, version } = latestPlatformRelease(releases, platform);
  console.log(
    `Latest ${platform.label} download: ${release.tag_name}/${asset.name}`,
  );
  if (mode !== "--publish") return;
  if (!/^sha256:[0-9a-f]{64}$/u.test(asset.digest ?? "")) {
    throw new Error(
      `The published ${platform.label} download has no SHA-256 digest.`,
    );
  }

  const alias = releases.find(
    (candidate) => candidate.tag_name === platform.aliasTag,
  );
  if (alias && (alias.draft || !alias.prerelease)) {
    throw new Error(
      "Refusing to replace a release that is not a published download alias.",
    );
  }
  const title = `Latest Rebellion 2 ${platform.label} download (v${version})`;
  if (
    alias?.name === title &&
    alias.assets.some(
      (candidate) =>
        candidate.name === platform.assetName &&
        candidate.state === "uploaded" &&
        candidate.digest === asset.digest &&
        candidate.size === asset.size,
    )
  ) {
    console.log(`The ${platform.label} download alias is already current.`);
    return;
  }

  const directory = await mkdtemp(
    join(tmpdir(), `reb2-${platform.id}-download-`),
  );
  try {
    github(
      "release",
      "download",
      release.tag_name,
      "--repo",
      repository,
      "--pattern",
      asset.name,
      "--dir",
      directory,
    );
    const sourceDownload = join(directory, asset.name);
    verifyPlatformDownload(await readFile(sourceDownload), asset, platform);
    const download = join(directory, platform.assetName);
    if (sourceDownload !== download) await rename(sourceDownload, download);
    const notes = join(directory, "notes.md");
    await writeFile(
      notes,
      `Download for the newest published ${platform.label} game release: [Rebellion 2 ${version}](https://github.com/${repository}/releases/tag/${release.tag_name}).\n`,
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
        platform.aliasTag,
        download,
        "--repo",
        repository,
        "--clobber",
      );
      github("release", "edit", platform.aliasTag, ...metadata);
    } else {
      github(
        "release",
        "create",
        platform.aliasTag,
        download,
        "--target",
        release.target_commitish,
        ...metadata,
      );
    }
    const published = JSON.parse(
      github("api", `repos/${repository}/releases/tags/${platform.aliasTag}`),
    );
    const publishedAsset = published.assets.find(
      (candidate) => candidate.name === platform.assetName,
    );
    if (
      published.draft ||
      !published.prerelease ||
      publishedAsset?.state !== "uploaded" ||
      publishedAsset?.digest !== asset.digest ||
      publishedAsset?.size !== asset.size
    ) {
      throw new Error(
        `The published ${platform.label} download alias does not match the selected release.`,
      );
    }
    console.log(
      `Published https://github.com/${repository}/releases/download/${platform.aliasTag}/${platform.assetName}`,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}
