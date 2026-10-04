import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { packageUpdateLayer } from "./package-update-layer.mjs";

test("packageUpdateLayer records paths, hashes, and sizes", async () => {
  const temporary = await mkdtemp(path.join(os.tmpdir(), "reb2-layer-"));
  const root = path.join(temporary, "root");
  const manifest = path.join(temporary, "manifest.json");
  const blobs = path.join(temporary, "blobs");
  await mkdir(path.join(root, "nested"), { recursive: true });
  await writeFile(path.join(root, "nested", "data.txt"), "data");
  await writeFile(path.join(root, "run"), "executable");

  await packageUpdateLayer(root, "1.2.3", manifest, blobs);

  const result = JSON.parse(await readFile(manifest, "utf8"));
  assert.equal(result.version, "1.2.3");
  assert.deepEqual(result.files.map((entry) => entry.path), ["nested/data.txt", "run"]);
  for (const entry of result.files) {
    assert.equal((await readFile(path.join(blobs, entry.sha256))).length, entry.size);
  }
});

test("packageUpdateLayer rejects symbolic links", async () => {
  const temporary = await mkdtemp(path.join(os.tmpdir(), "reb2-layer-"));
  const root = path.join(temporary, "root");
  await mkdir(root);
  await writeFile(path.join(root, "target"), "data");
  await symlink("target", path.join(root, "link"));

  await assert.rejects(
    packageUpdateLayer(
      root,
      "1.2.3",
      path.join(temporary, "manifest.json"),
      path.join(temporary, "blobs"),
    ),
    /cannot contain symbolic links/u,
  );
});
