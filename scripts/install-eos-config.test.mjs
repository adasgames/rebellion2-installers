import assert from "node:assert/strict";
import { mkdtemp, readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { installEosConfig } from "./install-eos-config.mjs";

test("installEosConfig writes every validated platform configuration", async () => {
  const game = await mkdtemp(path.join(os.tmpdir(), "reb2-eos-"));
  const environment = validEnvironment();
  await installEosConfig(game, environment);

  const destination = path.join(game, "Assets", "StreamingAssets", "EOS");
  for (const filename of [
    "eos_product_config.json",
    "eos_windows_config.json",
    "eos_macos_config.json",
    "eos_linux_config.json",
  ]) {
    const config = JSON.parse(
      await readFile(path.join(destination, filename), "utf8"),
    );
    assert.equal(typeof config, "object");
  }
});

test("installEosConfig rejects a missing secret", async () => {
  const game = await mkdtemp(path.join(os.tmpdir(), "reb2-eos-"));
  const environment = validEnvironment();
  delete environment.EOS_LINUX_CONFIG;
  await assert.rejects(
    installEosConfig(game, environment),
    /EOS_LINUX_CONFIG is missing/u,
  );
});

test("installEosConfig rejects incomplete credentials", async () => {
  const game = await mkdtemp(path.join(os.tmpdir(), "reb2-eos-"));
  const environment = validEnvironment();
  const windows = JSON.parse(environment.EOS_WINDOWS_CONFIG);
  windows.clientCredentials.ClientSecret = "";
  environment.EOS_WINDOWS_CONFIG = JSON.stringify(windows);
  await assert.rejects(
    installEosConfig(game, environment),
    /EOS_WINDOWS_CONFIG is missing clientCredentials.ClientSecret/u,
  );
});

function validEnvironment() {
  const platform = JSON.stringify({
    deployment: { SandboxId: { Value: "sandbox" }, DeploymentId: "deployment" },
    clientCredentials: {
      ClientId: "client",
      ClientSecret: "secret",
      EncryptionKey: "encryption",
    },
  });
  return {
    EOS_PRODUCT_CONFIG: JSON.stringify({
      ProductId: "product",
      ProductName: "Rebellion 2",
      Environments: {},
    }),
    EOS_WINDOWS_CONFIG: platform,
    EOS_MACOS_CONFIG: platform,
    EOS_LINUX_CONFIG: platform,
  };
}
