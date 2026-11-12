import { access, cp, mkdir, rm } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

/** Installs protected build assets into a checked-out game project. */
export async function installBuildAssets(gameDirectory) {
  const gameRoot = path.resolve(gameDirectory);
  const sourceRoot = path.join(gameRoot, ".ci", "assets");
  const assetsRoot = path.join(gameRoot, "Assets");
  const contentRoot = path.join(assetsRoot, "Content");

  await requireFile(
    path.join(
      sourceRoot,
      "Content",
      "Application",
      "Schemas",
      "game-config.xsd",
    ),
  );
  await rm(contentRoot, { recursive: true, force: true });
  await mkdir(path.join(contentRoot, "Packs"), { recursive: true });
  await copyTree(
    path.join(sourceRoot, "Content", "Application"),
    path.join(contentRoot, "Application"),
    true,
    true,
  );
  await cp(
    path.join(sourceRoot, "Content", "catalog.xml"),
    path.join(contentRoot, "catalog.xml"),
  );
  await copyTree(
    path.join(sourceRoot, "Content", "Packs", "ClassicGalacticCivilWar"),
    path.join(contentRoot, "Packs", "ClassicGalacticCivilWar"),
    true,
    true,
  );
  await copyTree(
    path.join(sourceRoot, "Models", "MainMenu"),
    path.join(assetsRoot, "Art", "Models", "MainMenu"),
    true,
    true,
  );
  await copyTree(
    path.join(sourceRoot, "UnityAssets"),
    assetsRoot,
    false,
    false,
  );
  await requireFile(
    path.join(
      assetsRoot,
      "Art",
      "MainMenu",
      "Planet",
      "Flare",
      "PurpleFlareMesh.asset",
    ),
  );
  await rm(path.join(gameRoot, ".ci"), { recursive: true, force: true });
}

/** Copies one asset tree while applying release exclusions. */
async function copyTree(
  source,
  destination,
  excludeMetadata,
  replaceDestination,
) {
  if (replaceDestination) {
    await rm(destination, { recursive: true, force: true });
  }
  await cp(source, destination, {
    recursive: true,
    filter: (entry) => {
      const name = path.basename(entry);
      return (
        name !== ".DS_Store" && (!excludeMetadata || !name.endsWith(".meta"))
      );
    },
  });
}

/** Requires a regular release input to exist. */
async function requireFile(filePath) {
  try {
    await access(filePath);
  } catch {
    throw new Error(`Required build asset is missing: ${filePath}`);
  }
}

async function main() {
  const [gameDirectory] = process.argv.slice(2);
  if (!gameDirectory) {
    throw new Error(
      "Usage: node scripts/install-build-assets.mjs <game-directory>",
    );
  }
  await installBuildAssets(gameDirectory);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await main();
}
