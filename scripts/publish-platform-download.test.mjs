import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import test from "node:test";

const bytes = Buffer.from("verified game package");
const digest = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;

/** Runs the real publisher CLI against isolated release and asset fixtures. */
async function runPublisher(platform, { alias, changedBytes = false } = {}) {
  const directory = await mkdtemp(
    join(tmpdir(), "reb2-download-publisher-test-"),
  );
  const sourceName =
    platform === "linux"
      ? "Rebellion2-0.1.5-x86_64.AppImage"
      : "Rebellion2-macOS.zip";
  const assetName =
    platform === "linux" ? "Rebellion2-Linux-x86_64.AppImage" : sourceName;
  const asset = {
    name: assetName,
    state: "uploaded",
    size: bytes.length,
    digest,
  };
  const published = {
    tag_name: `latest-${platform}-download`,
    name: `Latest Rebellion 2 ${platform === "linux" ? "Linux" : "macOS"} download (v0.1.5)`,
    draft: false,
    prerelease: true,
    assets: [asset],
  };
  const releases = [
    {
      tag_name: "v0.1.6",
      draft: false,
      prerelease: false,
      assets: [{ name: "Rebellion2-Windows-Setup.exe" }],
    },
    {
      tag_name: "v0.1.5",
      target_commitish: "released-player",
      draft: false,
      prerelease: false,
      assets: [{ ...asset, name: sourceName }],
    },
  ];
  if (alias) {
    releases.push(
      alias === "current"
        ? published
        : { ...published, name: "Earlier platform download", assets: [] },
    );
  }
  try {
    await writeFile(
      join(directory, "fixture.json"),
      JSON.stringify({
        releases,
        published,
        sourceName,
        bytes: (changedBytes ? Buffer.alloc(bytes.length) : bytes).toString(
          "base64",
        ),
      }),
    );
    await writeFile(
      join(directory, "gh"),
      `#!${process.execPath}
const fs = require("node:fs");
const path = require("node:path");
const root = process.env.DOWNLOAD_PUBLISHER_FIXTURE;
const fixture = JSON.parse(fs.readFileSync(path.join(root, "fixture.json"), "utf8"));
const args = process.argv.slice(2);
fs.appendFileSync(path.join(root, "calls.jsonl"), JSON.stringify(args) + "\\n");
if (args[0] === "api") {
  if (args[1] === "--paginate") {
    console.log(fixture.releases.map(release => JSON.stringify(release)).join("\\n"));
  } else {
    console.log(JSON.stringify(fixture.published));
  }
} else if (args[1] === "download") {
  const target = args[args.indexOf("--dir") + 1];
  if (args[args.indexOf("--pattern") + 1] !== fixture.sourceName) process.exit(2);
  fs.writeFileSync(path.join(target, fixture.sourceName), Buffer.from(fixture.bytes, "base64"));
} else if (args[1] === "create" || args[1] === "upload") {
  fs.copyFileSync(args[3], path.join(root, "uploaded.bin"));
  fs.writeFileSync(path.join(root, "uploaded-name.txt"), path.basename(args[3]));
}
`,
      { mode: 0o755 },
    );
    const result = spawnSync(
      process.execPath,
      [
        resolve(`scripts/publish-${platform}-download.mjs`),
        "example/releases",
        "--publish",
      ],
      {
        env: {
          ...process.env,
          PATH: [directory, process.env.PATH].join(delimiter),
          DOWNLOAD_PUBLISHER_FIXTURE: directory,
        },
        encoding: "utf8",
      },
    );
    const calls = (await readFile(join(directory, "calls.jsonl"), "utf8"))
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    const mutates = calls.filter(
      (args) =>
        args[0] === "release" && ["create", "upload", "edit"].includes(args[1]),
    );
    let uploaded;
    if (mutates.length) {
      uploaded = {
        name: await readFile(join(directory, "uploaded-name.txt"), "utf8"),
        bytes: await readFile(join(directory, "uploaded.bin")),
      };
    }
    return { result, calls, mutates, uploaded };
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("Linux publication copies the verified versioned package to the fixed download name", async () => {
  const { result, mutates, uploaded } = await runPublisher("linux");
  assert.equal(result.status, 0, result.stderr);
  assert.equal(uploaded.name, "Rebellion2-Linux-x86_64.AppImage");
  assert.deepEqual(uploaded.bytes, bytes);
  assert.equal(mutates.length, 1);
  assert.deepEqual(mutates[0].slice(0, 3), [
    "release",
    "create",
    "latest-linux-download",
  ]);
  assert.ok(mutates[0].includes("--prerelease"));
  assert.ok(mutates[0].includes("--latest=false"));
});

test("damaged Linux bytes fail before changing the public download", async () => {
  const { result, mutates } = await runPublisher("linux", {
    changedBytes: true,
  });
  assert.notEqual(result.status, 0);
  assert.match(
    result.stderr,
    /does not match its published size and SHA-256 digest/u,
  );
  assert.deepEqual(mutates, []);
});

test("a stale Linux alias is refreshed with the verified package", async () => {
  const { result, mutates, uploaded } = await runPublisher("linux", {
    alias: "old",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(uploaded.bytes, bytes);
  assert.equal(uploaded.name, "Rebellion2-Linux-x86_64.AppImage");
  assert.deepEqual(
    mutates.map((args) => args.slice(0, 3)),
    [
      ["release", "upload", "latest-linux-download"],
      ["release", "edit", "latest-linux-download"],
    ],
  );
});

test("the current macOS alias requires no publication", async () => {
  const { result, mutates, calls } = await runPublisher("macos", {
    alias: "current",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(mutates, []);
  assert.ok(calls.every((args) => args[0] === "api"));
});

test("macOS publication preserves its fixed archive name and verified bytes", async () => {
  const { result, uploaded } = await runPublisher("macos");
  assert.equal(result.status, 0, result.stderr);
  assert.equal(uploaded.name, "Rebellion2-macOS.zip");
  assert.deepEqual(uploaded.bytes, bytes);
});
