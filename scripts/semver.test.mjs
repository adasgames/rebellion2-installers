import assert from "node:assert/strict";
import test from "node:test";

import { compareSemanticVersions } from "./semver.mjs";

test("compareSemanticVersions orders stable numeric versions", () => {
  assert.equal(compareSemanticVersions("1.2.3", "1.2.2"), 1);
  assert.equal(compareSemanticVersions("1.2.3", "1.2.3"), 0);
  assert.equal(compareSemanticVersions("1.2.3", "2.0.0"), -1);
});

test("compareSemanticVersions orders prereleases before stable releases", () => {
  assert.equal(compareSemanticVersions("1.0.0-beta.2", "1.0.0-beta.1"), 1);
  assert.equal(compareSemanticVersions("1.0.0-beta", "1.0.0"), -1);
});

test("compareSemanticVersions ignores build metadata", () => {
  assert.equal(compareSemanticVersions("1.0.0+build.2", "1.0.0+build.1"), 0);
});
