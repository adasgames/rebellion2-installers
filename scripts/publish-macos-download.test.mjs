import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import { latestMacRelease, verifyMacDownload } from "./publish-macos-download.mjs";

const bytes = Buffer.from("published macOS build");
const asset = {
  name: "Rebellion2-macOS.zip",
  state: "uploaded",
  size: bytes.length,
  digest: `sha256:${createHash("sha256").update(bytes).digest("hex")}`,
};

/** Builds release fixtures with explicit macOS availability. */
function release(version, macos = true, overrides = {}) {
  return {
    tag_name: `v${version}`,
    draft: false,
    prerelease: false,
    assets: macos ? [{ ...asset }] : [{ name: "Rebellion2-Windows-Setup.exe" }],
    ...overrides,
  };
}

test("a newer Windows release preserves the latest macOS download", () => {
  const selected = latestMacRelease([release("0.1.6", false), release("0.1.5")]);
  assert.equal(selected.release.tag_name, "v0.1.5");
});

test("a newer macOS release advances the download", () => {
  const selected = latestMacRelease([release("0.1.5"), release("0.1.7")]);
  assert.equal(selected.version, "0.1.7");
});

test("an older backfill cannot move the macOS download backward", () => {
  const selected = latestMacRelease([
    release("0.1.10"),
    release("0.1.9"),
    release("0.1.4"),
  ]);
  assert.equal(selected.version, "0.1.10");
});

test("drafts, previews, launchers and aliases cannot become the stable macOS download", () => {
  const releases = [
    release("9.0.0", true, { draft: true }),
    release("8.0.0", true, { prerelease: true }),
    release("7.0.0-preview.1"),
    release("07.0.0"),
    release("6.0.0", true, { tag_name: "launcher-v6.0.0" }),
    release("5.0.0", true, { tag_name: "latest-macos" }),
    release("4.0.0", true, { tag_name: "latest-macos-download" }),
    release("0.1.5"),
  ];
  assert.equal(latestMacRelease(releases).version, "0.1.5");
});

test("an incomplete asset does not replace a usable macOS download", () => {
  const incomplete = release("0.1.6", true, { assets: [{ ...asset, state: "new" }] });
  assert.equal(latestMacRelease([incomplete, release("0.1.5")]).version, "0.1.5");
});

test("an empty asset does not replace a usable macOS download", () => {
  const empty = release("0.1.6", true, { assets: [{ ...asset, size: 0 }] });
  assert.equal(latestMacRelease([empty, release("0.1.5")]).version, "0.1.5");
});

test("missing macOS downloads fail without selecting another platform", () => {
  assert.throws(
    () => latestMacRelease([release("0.1.6", false)]),
    /No published stable macOS/u,
  );
});

test("the published macOS bytes pass verification", () => {
  assert.doesNotThrow(() => verifyMacDownload(bytes, asset));
});

test("changed macOS bytes fail verification even at the same size", () => {
  const changed = Buffer.from(bytes);
  changed[0] ^= 1;
  assert.throws(() => verifyMacDownload(changed, asset), /does not match/u);
});

test("incomplete macOS downloads fail verification", () => {
  assert.throws(() => verifyMacDownload(bytes.subarray(1), asset), /does not match/u);
});
