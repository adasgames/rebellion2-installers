import { createHash } from "node:crypto";
import { copyFile, mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

/** Builds a signed-manifest input and content-addressed blobs from one file tree. */
export async function packageUpdateLayer(root, version, manifestPath, blobsDirectory) {
  const files = [];
  await collectFiles(root, root, files);
  if (files.length === 0) {
    throw new Error("Update layer cannot be empty.");
  }

  await mkdir(blobsDirectory, { recursive: true });
  const entries = [];
  for (const absolutePath of files.sort()) {
    const bytes = await readFile(absolutePath);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    const relativePath = path.relative(root, absolutePath).split(path.sep).join("/");
    const entry = { path: relativePath, sha256, size: bytes.length };
    entries.push(entry);
    const blobPath = path.join(blobsDirectory, sha256);
    try {
      await stat(blobPath);
    } catch {
      await copyFile(absolutePath, blobPath);
    }
  }

  await mkdir(path.dirname(manifestPath), { recursive: true });
  await writeFile(
    manifestPath,
    `${JSON.stringify({ version, files: entries })}\n`,
  );
}

/** Recursively collects regular files and rejects symbolic links. */
async function collectFiles(root, directory, files) {
  const entries = await readdir(directory, { withFileTypes: true });
  for (const entry of entries) {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isSymbolicLink()) {
      throw new Error(`Update layers cannot contain symbolic links: ${path.relative(root, absolutePath)}`);
    }
    if (entry.isDirectory()) {
      await collectFiles(root, absolutePath, files);
    } else if (entry.isFile()) {
      files.push(absolutePath);
    }
  }
}

async function main() {
  const [root, version, manifestPath, blobsDirectory] = process.argv.slice(2);
  if (!root || !version || !manifestPath || !blobsDirectory) {
    throw new Error(
      "Usage: node scripts/package-update-layer.mjs <root> <version> <manifest.json> <blobs-directory>",
    );
  }
  await packageUpdateLayer(root, version, manifestPath, blobsDirectory);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
