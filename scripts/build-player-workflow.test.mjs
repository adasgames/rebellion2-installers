import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const workflowUrl = new URL(
  "../.github/workflows/build-player.yml",
  import.meta.url,
);

test("build player checks out and invokes release tooling", async () => {
  const workflow = await readFile(workflowUrl, "utf8");
  const checkoutIndex = workflow.indexOf("- name: Checkout build tooling");
  const assetInstallIndex = workflow.indexOf(
    "node tooling/scripts/install-build-assets.mjs game",
  );
  const eosInstallIndex = workflow.indexOf(
    "node tooling/scripts/install-eos-config.mjs game",
  );

  assert.notEqual(checkoutIndex, -1);
  assert.match(workflow, /repository: adasgames\/rebellion2-installers/u);
  assert.match(workflow, /ref: \$\{\{ github\.sha \}\}/u);
  assert.match(workflow, /path: tooling/u);
  assert.ok(checkoutIndex < assetInstallIndex);
  assert.ok(checkoutIndex < eosInstallIndex);
});
