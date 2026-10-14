import assert from "node:assert/strict";
import test from "node:test";

import { updateContentPackCatalog } from "./update-content-pack-catalog.mjs";

function release(overrides = {}) {
  return {
    id: "sample-pack",
    name: "Sample Pack",
    description: "A sample.",
    version: "1.2.3",
    directory: "SamplePack",
    manifest: "dist/content-packs/sample-pack/manifest-1.2.3.json",
    blobs: "content-pack-blobs/",
    basePackId: "classic-galactic-civil-war",
    ...overrides,
  };
}

function manifest() {
  return {
    version: "1.2.3",
    files: [
      { path: "pack.xml", sha256: "a", size: 10 },
      { path: "Data/units.xml", sha256: "b", size: 25 },
    ],
  };
}

test("catalog records one independently versioned pack and its size", () => {
  const result = updateContentPackCatalog(
    { schemaVersion: 1, packs: [] },
    release(),
    manifest(),
  );

  assert.equal(result.packs.length, 1);
  assert.equal(result.packs[0].downloadSize, 35);
  assert.equal(result.packs[0].version, "1.2.3");
});

test("publishing a new pack version replaces only that catalog entry", () => {
  const existing = {
    schemaVersion: 1,
    packs: [release(), release({ id: "other-pack", name: "Other", directory: "OtherPack" })],
  };

  const result = updateContentPackCatalog(
    existing,
    release({ version: "2.0.0", manifest: "dist/content-packs/sample-pack/manifest-2.0.0.json" }),
    { ...manifest(), version: "2.0.0" },
  );

  assert.equal(result.packs.length, 2);
  assert.equal(result.packs.find((pack) => pack.id === "sample-pack").version, "2.0.0");
  assert.equal(result.packs.find((pack) => pack.id === "other-pack").version, "1.2.3");
});

test("catalog rejects a directory owned by another pack", () => {
  const existing = {
    schemaVersion: 1,
    packs: [release({ id: "other-pack", directory: "SharedDirectory" })],
  };

  assert.throws(
    () => updateContentPackCatalog(existing, release({ directory: "SharedDirectory" }), manifest()),
    /already belongs/u,
  );
});
