import assert from "node:assert/strict";
import { access, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { installBuildAssets } from "./install-build-assets.mjs";

test("installBuildAssets installs release inputs and applies exclusions", async () => {
  const game = await createGameFixture();
  await installBuildAssets(game);

  assert.equal(
    await readFile(
      path.join(game, "Assets", "Content", "Application", "data.xml"),
      "utf8",
    ),
    "application",
  );
  assert.equal(
    await readFile(path.join(game, "Assets", "Content", "catalog.xml"), "utf8"),
    "catalog",
  );
  assert.equal(
    await readFile(
      path.join(
        game,
        "Assets",
        "Content",
        "Packs",
        "ClassicGalacticCivilWar",
        "pack.xml",
      ),
      "utf8",
    ),
    "pack",
  );
  assert.equal(
    await readFile(
      path.join(game, "Assets", "Art", "Models", "MainMenu", "planet.glb"),
      "utf8",
    ),
    "model",
  );
  assert.equal(
    await readFile(
      path.join(game, "Assets", "Shared", "asset.bin.meta"),
      "utf8",
    ),
    "metadata",
  );
  await assert.rejects(
    access(
      path.join(game, "Assets", "Content", "Application", "data.xml.meta"),
    ),
  );
  await assert.rejects(access(path.join(game, ".ci")));
});

test("installBuildAssets rejects incomplete protected assets", async () => {
  const game = await mkdtemp(path.join(os.tmpdir(), "reb2-assets-"));
  await assert.rejects(
    installBuildAssets(game),
    /Required build asset is missing/u,
  );
});

async function createGameFixture() {
  const game = await mkdtemp(path.join(os.tmpdir(), "reb2-assets-"));
  const source = path.join(game, ".ci", "assets");
  const files = new Map([
    ["Content/Application/Schemas/game-config.xsd", "schema"],
    ["Content/Application/data.xml", "application"],
    ["Content/Application/data.xml.meta", "excluded"],
    ["Content/catalog.xml", "catalog"],
    ["Content/Packs/ClassicGalacticCivilWar/pack.xml", "pack"],
    ["Models/MainMenu/planet.glb", "model"],
    ["UnityAssets/Art/MainMenu/Planet/Flare/PurpleFlareMesh.asset", "flare"],
    ["UnityAssets/Shared/asset.bin.meta", "metadata"],
  ]);
  for (const [relativePath, contents] of files) {
    const filePath = path.join(source, ...relativePath.split("/"));
    await mkdir(path.dirname(filePath), { recursive: true });
    await writeFile(filePath, contents);
  }
  return game;
}
