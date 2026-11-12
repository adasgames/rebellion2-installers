import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

const configurations = [
  ["EOS_PRODUCT_CONFIG", "eos_product_config.json", validateProduct],
  ["EOS_WINDOWS_CONFIG", "eos_windows_config.json", validatePlatform],
  ["EOS_MACOS_CONFIG", "eos_macos_config.json", validatePlatform],
  ["EOS_LINUX_CONFIG", "eos_linux_config.json", validatePlatform],
];

/** Installs validated EOS release configuration into a checked-out game project. */
export async function installEosConfig(
  gameDirectory,
  environment = process.env,
) {
  const destination = path.join(
    path.resolve(gameDirectory),
    "Assets",
    "StreamingAssets",
    "EOS",
  );
  await mkdir(destination, { recursive: true });
  for (const [variable, filename, validate] of configurations) {
    const source = environment[variable];
    if (!source) {
      throw new Error(`Required release secret ${variable} is missing.`);
    }
    let config;
    try {
      config = JSON.parse(source);
    } catch {
      throw new Error(`Release secret ${variable} is not valid JSON.`);
    }
    validate(config, variable);
    await writeFile(
      path.join(destination, filename),
      `${JSON.stringify(config, null, 2)}\n`,
    );
  }
}

/** Validates the EOS product fields consumed during runtime initialization. */
function validateProduct(config, variable) {
  requireText(config.ProductId, variable, "ProductId");
  requireText(config.ProductName, variable, "ProductName");
  if (!config.Environments || typeof config.Environments !== "object") {
    throw new Error(`${variable} is missing Environments.`);
  }
}

/** Validates the EOS deployment and client fields consumed by every player target. */
function validatePlatform(config, variable) {
  requireText(
    config.deployment?.SandboxId?.Value,
    variable,
    "deployment.SandboxId.Value",
  );
  requireText(
    config.deployment?.DeploymentId,
    variable,
    "deployment.DeploymentId",
  );
  requireText(
    config.clientCredentials?.ClientId,
    variable,
    "clientCredentials.ClientId",
  );
  requireText(
    config.clientCredentials?.ClientSecret,
    variable,
    "clientCredentials.ClientSecret",
  );
  requireText(
    config.clientCredentials?.EncryptionKey,
    variable,
    "clientCredentials.EncryptionKey",
  );
}

/** Requires a populated configuration string without logging its value. */
function requireText(value, variable, field) {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`${variable} is missing ${field}.`);
  }
}

async function main() {
  const [gameDirectory] = process.argv.slice(2);
  if (!gameDirectory) {
    throw new Error(
      "Usage: node scripts/install-eos-config.mjs <game-directory>",
    );
  }
  await installEosConfig(gameDirectory);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await main();
}
