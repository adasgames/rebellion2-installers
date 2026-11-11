import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import {
  latestLinuxRelease,
  verifyLinuxDownload,
} from "./publish-linux-download.mjs";

const bytes = Buffer.from("published Linux build");
const asset = {
  name: "Rebellion2-0.1.5-x86_64.AppImage",
  state: "uploaded",
  size: bytes.length,
  digest: `sha256:${createHash("sha256").update(bytes).digest("hex")}`,
};

/** Builds release fixtures with explicit Linux availability. */
function release(version, linux = true, overrides = {}) {
  return {
    tag_name: `v${version}`,
    draft: false,
    prerelease: false,
    assets: linux
      ? [{ ...asset, name: `Rebellion2-${version}-x86_64.AppImage` }]
      : [{ name: "Rebellion2-Windows-Setup.exe" }],
    ...overrides,
  };
}

test("a newer Windows release preserves the latest Linux download", () => {
  const selected = latestLinuxRelease([
    release("0.1.6", false),
    release("0.1.5"),
  ]);
  assert.equal(selected.release.tag_name, "v0.1.5");
});

test("a newer Linux release advances the download", () => {
  const selected = latestLinuxRelease([release("0.1.5"), release("0.1.7")]);
  assert.equal(selected.version, "0.1.7");
});

test("an older backfill cannot move the Linux download backward", () => {
  const selected = latestLinuxRelease([
    release("0.1.10"),
    release("0.1.9"),
    release("0.1.4"),
  ]);
  assert.equal(selected.version, "0.1.10");
});

test("drafts, previews, launchers and aliases cannot become the stable Linux download", () => {
  const releases = [
    release("9.0.0", true, { draft: true }),
    release("8.0.0", true, { prerelease: true }),
    release("7.0.0-preview.1"),
    release("07.0.0"),
    release("6.0.0", true, { tag_name: "launcher-v6.0.0" }),
    release("5.0.0", true, { tag_name: "latest-linux" }),
    release("4.0.0", true, { tag_name: "latest-linux-download" }),
    release("0.1.5"),
  ];
  assert.equal(latestLinuxRelease(releases).version, "0.1.5");
});

test("an incomplete asset does not replace a usable Linux download", () => {
  const incomplete = release("0.1.6");
  incomplete.assets[0].state = "new";
  assert.equal(
    latestLinuxRelease([incomplete, release("0.1.5")]).version,
    "0.1.5",
  );
});

test("an empty asset does not replace a usable Linux download", () => {
  const empty = release("0.1.6");
  empty.assets[0].size = 0;
  assert.equal(latestLinuxRelease([empty, release("0.1.5")]).version, "0.1.5");
});

test("missing Linux downloads fail without selecting another platform", () => {
  assert.throws(
    () => latestLinuxRelease([release("0.1.6", false)]),
    /No published stable Linux/u,
  );
});

test("the published Linux bytes pass verification", () => {
  assert.doesNotThrow(() => verifyLinuxDownload(bytes, asset));
});

test("changed Linux bytes fail verification even at the same size", () => {
  const changed = Buffer.from(bytes);
  changed[0] ^= 1;
  assert.throws(() => verifyLinuxDownload(changed, asset), /does not match/u);
});

test("incomplete Linux downloads fail verification", () => {
  assert.throws(
    () => verifyLinuxDownload(bytes.subarray(1), asset),
    /does not match/u,
  );
});

test("another architecture does not replace a usable Linux download", () => {
  const other = release("0.1.6");
  other.assets[0].name = "Rebellion2-0.1.6-aarch64.AppImage";
  assert.equal(latestLinuxRelease([other, release("0.1.5")]).version, "0.1.5");
});

test("a mismatched asset version does not replace a usable Linux download", () => {
  const other = release("0.1.6");
  other.assets[0].name = "Rebellion2-0.1.4-x86_64.AppImage";
  assert.equal(latestLinuxRelease([other, release("0.1.5")]).version, "0.1.5");
});
