import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

test("Linux AppRun seeds writable state without replacing installed versions", () => {
  const result = spawnSync("bash", ["packaging/linux/test-apprun.sh"], {
    cwd: process.cwd(),
    encoding: "utf8",
  });

  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
});
