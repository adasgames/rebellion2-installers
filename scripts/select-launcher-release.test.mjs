import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { selectLauncherRelease } from "./select-launcher-release.mjs";

/** Creates published launcher metadata with one nonempty uploaded platform bundle. */
function release(version, platform, overrides = {}) {
  const names = {
    windows: "Windows.zip",
    macos: "macOS.zip",
    linux: "Linux.tar.gz",
  };
  return {
    tag_name: `launcher-v${version}`,
    assets: [
      {
        name: `Rebellion2-launcher-${names[platform]}`,
        state: "uploaded",
        size: 1,
      },
    ],
    ...overrides,
  };
}

test("selectLauncherRelease keeps the current platform's declared launcher", () => {
  const channel = {
    version: "1.3.3",
    platforms: { windows: { manifest: "dist/current-windows.json" } },
  };
  assert.deepEqual(selectLauncherRelease(channel, [], "windows"), {
    version: "1.3.3",
    manifest: "dist/current-windows.json",
  });
});

for (const platform of ["macos", "linux"]) {
  test(`selectLauncherRelease keeps ${platform} installers on their published launcher after a Windows-only release`, () => {
    const channel = {
      version: "1.3.3",
      platforms: { windows: { manifest: "dist/windows.json" } },
    };
    const releases = [release("1.3.3", "windows"), release("1.3.2", platform)];
    assert.deepEqual(selectLauncherRelease(channel, releases, platform), {
      version: "1.3.2",
      manifest: `dist/launcher-manifest-${platform}-1.3.2.json`,
    });
  });
}

test("selectLauncherRelease ignores game releases, draft bundles and prereleases", () => {
  const releases = [
    release("9.0.0", "macos", { tag_name: "v9.0.0" }),
    release("1.4.0", "macos", { draft: true }),
    release("1.3.9", "macos", { prerelease: true }),
    release("1.3.3", "windows"),
    release("1.3.2", "macos"),
    release("1.3.1", "macos"),
  ];
  assert.equal(selectLauncherRelease({}, releases, "macos").version, "1.3.2");
});

test("selectLauncherRelease rejects missing or empty platform downloads", () => {
  const releases = [
    release("1.3.3", "windows"),
    release("1.3.2", "macos", { assets: [] }),
  ];
  assert.throws(
    () => selectLauncherRelease({}, releases, "macos"),
    /No published stable macos launcher/u,
  );
});

test("selectLauncherRelease rejects unsupported platforms", () => {
  assert.throws(
    () => selectLauncherRelease({}, [], "other"),
    /Unsupported launcher platform/u,
  );
});

test("selectLauncherRelease rejects malformed declarations rather than selecting an older release", () => {
  for (const channel of [
    { version: "", platforms: { windows: { manifest: "dist/windows.json" } } },
    { version: "1.3.3", platforms: { windows: {} } },
    { version: "1.3.3", platforms: { windows: { manifest: "" } } },
  ]) {
    assert.throws(
      () =>
        selectLauncherRelease(
          channel,
          [release("1.3.2", "windows")],
          "windows",
        ),
      /Invalid windows launcher channel/u,
    );
  }
});

test("selectLauncherRelease ignores incomplete uploaded bundles", () => {
  const releases = [
    release("1.3.3", "macos", {
      assets: [
        { name: "Rebellion2-launcher-macOS.zip", state: "uploaded", size: 0 },
      ],
    }),
    release("1.3.2", "macos"),
  ];
  assert.equal(selectLauncherRelease({}, releases, "macos").version, "1.3.2");
});

test("selectLauncherRelease CLI writes the selected platform manifest", async () => {
  const directory = await mkdtemp(join(tmpdir(), "launcher-selection-"));
  try {
    const channelPath = join(directory, "channel.json");
    const releasesPath = join(directory, "releases.json");
    const outputPath = join(directory, "selection.json");
    await writeFile(
      channelPath,
      JSON.stringify({
        version: "1.3.3",
        platforms: { windows: { manifest: "dist/windows.json" } },
      }),
    );
    await writeFile(releasesPath, JSON.stringify([release("1.3.2", "macos")]));
    execFileSync(process.execPath, [
      fileURLToPath(new URL("./select-launcher-release.mjs", import.meta.url)),
      channelPath,
      releasesPath,
      "macos",
      outputPath,
    ]);
    assert.deepEqual(JSON.parse(await readFile(outputPath, "utf8")), {
      version: "1.3.2",
      manifest: "dist/launcher-manifest-macos-1.3.2.json",
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
