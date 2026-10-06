import assert from "node:assert/strict";
import test from "node:test";

import { addGamePlatform } from "./add-game-platform.mjs";

const content = {
  version: "0.0.26",
  manifest: "dist/manifest-0.0.26.json",
  blobs: "blobs/",
};
const linux = [
  "0.0.26",
  "linux",
  "dist/game-manifest-linux-0.0.26.json",
  "game-blobs/",
  "signed",
];

test("addGamePlatform preserves existing game platforms", () => {
  const source = {
    version: "0.0.26",
    platforms: {
      windows: {
        manifest: "dist/game-manifest-windows-0.0.26.json",
        blobs: "game-blobs/",
        signature: "windows-signed",
      },
      macos: {
        manifest: "dist/game-manifest-macos-0.0.26.json",
        blobs: "game-blobs/",
        signature: "macos-signed",
      },
    },
    content,
  };
  const original = structuredClone(source);

  const result = addGamePlatform(source, ...linux);

  assert.deepEqual(result.platforms.windows, source.platforms.windows);
  assert.deepEqual(result.platforms.macos, source.platforms.macos);
  assert.equal(result.platforms.linux.signature, "signed");
  assert.deepEqual(source, original);
});

test("addGamePlatform rejects a release-version mismatch", () => {
  const source = { version: "0.0.25", platforms: {}, content };
  assert.throws(
    () => addGamePlatform(source, ...linux),
    /versions must match/u,
  );
});

test("addGamePlatform rejects a content-version mismatch", () => {
  const source = {
    version: "0.0.26",
    platforms: {},
    content: { ...content, version: "0.0.25" },
  };
  assert.throws(
    () => addGamePlatform(source, ...linux),
    /versions must match/u,
  );
});

test("addGamePlatform rejects an existing platform", () => {
  const source = {
    version: "0.0.26",
    platforms: { linux: { signature: "existing" } },
    content,
  };
  assert.throws(
    () => addGamePlatform(source, ...linux),
    /already supports linux/u,
  );
});

test("addGamePlatform rejects malformed game platforms", () => {
  const source = { version: "0.0.26", platforms: [], content };
  assert.throws(
    () => addGamePlatform(source, ...linux),
    /platforms must be an object/u,
  );
});
