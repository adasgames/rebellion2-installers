import assert from "node:assert/strict";
import {
  createHash,
  createPrivateKey,
  createPublicKey,
  sign,
} from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import * as fs from "node:fs/promises";
import { watch } from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Drives a real Windows launcher through WebView2's local debugging interface.
// All release, ownership, archive, and blob responses come from this loopback server.
if (process.platform !== "win32")
  throw new Error("This native runtime suite requires Windows/WebView2.");
const repository = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const scratch = await fs.mkdtemp(
  path.join(os.tmpdir(), "reb2-launcher-runtime-"),
);
const token = `${Buffer.from(JSON.stringify({ exp: 4102444800 })).toString("base64url")}.fixture`;
const privateKey = createPrivateKey({
  key: Buffer.concat([
    Buffer.from("302e020100300506032b657004220420", "hex"),
    Buffer.alloc(32, 7),
  ]),
  format: "der",
  type: "pkcs8",
});
const publicKey = createPublicKey(privateKey)
  .export({ format: "der", type: "spki" })
  .subarray(-32)
  .toString("hex");
const results = [];
const sessions = new Set();
let state;
let base;
let launcherBytes;
let helperBytes;
let gameBytes;
let installerPath;
let installedLauncherBytes;
let olderLauncherBytes;
let olderHelperBytes;
let olderBuild;
const installedSettings = Buffer.from("fresh installer settings");

const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const readText = (file) =>
  fs.readFile(file, "utf8").catch((error) => {
    if (error.code === "ENOENT") return null;
    throw error;
  });

async function until(check, description, timeout = 20000) {
  const deadline = Date.now() + timeout;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const result = await check();
      if (result) return result;
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  throw new Error(
    `Timed out waiting for ${description}${lastError ? `: ${lastError.message}` : ""}`,
  );
}

async function command(executable, args, options = {}) {
  const child = spawn(executable, args, {
    cwd: repository,
    windowsHide: true,
    ...options,
  });
  let output = "";
  child.stdout?.on("data", (chunk) => {
    output += chunk;
  });
  child.stderr?.on("data", (chunk) => {
    output += chunk;
  });
  const [code] = await once(child, "exit");
  if (code !== 0) throw new Error(`${executable} failed (${code}):\n${output}`);
  return output;
}

async function buildInstallerFixture() {
  const compiler =
    process.env.ISCC_PATH ||
    path.join(process.env.LOCALAPPDATA, "Programs", "Inno Setup 6", "ISCC.exe");
  await fs.access(compiler);
  const fixture = path.join(scratch, "installer-fixture");
  const player = path.join(fixture, "player");
  await fs.mkdir(path.join(player, "Rebellion2_Data"), { recursive: true });
  await fs.writeFile(path.join(player, "Rebellion2.exe"), gameBytes);
  await fs.writeFile(
    path.join(player, "Rebellion2_Data/runtime-settings.json"),
    installedSettings,
  );
  installedLauncherBytes = Buffer.concat([
    launcherBytes,
    Buffer.from("fixture installer release"),
  ]);
  const launcher = path.join(fixture, "rebellion2-launcher.exe");
  const helper = path.join(fixture, "rebellion2-update-helper.exe");
  await fs.writeFile(launcher, installedLauncherBytes);
  await fs.writeFile(helper, helperBytes);
  const launcherManifest = path.join(fixture, "launcher-manifest.json");
  const gameManifest = path.join(fixture, "game-manifest.json");
  const launcherVersion = path.join(fixture, "launcher-version.txt");
  const gameVersion = path.join(fixture, "game-version.txt");
  await fs.writeFile(
    launcherManifest,
    JSON.stringify(
      manifest(
        "1.2.0",
        new Map([
          ["rebellion2-launcher.exe", installedLauncherBytes],
          ["rebellion2-update-helper.exe", helperBytes],
        ]),
      ),
    ),
  );
  await fs.writeFile(
    gameManifest,
    JSON.stringify(
      manifest(
        "3.0.0",
        new Map([
          ["Rebellion2.exe", gameBytes],
          ["Rebellion2_Data/runtime-settings.json", installedSettings],
        ]),
      ),
    ),
  );
  await fs.writeFile(launcherVersion, "1.2.0");
  await fs.writeFile(gameVersion, "3.0.0");
  let script = process.env.REB2_TEST_INSTALLER_BASELINE
    ? await command("git", [
        "show",
        "HEAD:packaging/windows/rebellion2-launcher.iss",
      ])
    : await fs.readFile(
        path.join(repository, "packaging/windows/rebellion2-launcher.iss"),
        "utf8",
      );
  script = script
    .replace(/^AppId=.*$/m, `AppId=Reb2Runtime-${path.basename(scratch)}`)
    .replace("[Setup]", "[Setup]\nUninstallable=no\nCreateUninstallRegKey=no")
    .replace(/\[Icons\][\s\S]*?(?=\[UninstallDelete\])/, "")
    .replace(
      "  InstallationPrepared := True;",
      `  InstallationPrepared := True;
  if FileExists(ExpandConstant('{app}\\.test-installer-pause')) then begin
    SaveStringToFile(ExpandConstant('{app}\\.test-installer-held'), 'ready', False);
    while not FileExists(ExpandConstant('{app}\\.test-installer-release')) do Sleep(50);
  end;`,
    );
  const scriptPath = path.join(fixture, "fixture.iss");
  await fs.writeFile(scriptPath, script);
  await command(compiler, [
    "/Qp",
    `/DGameDir=${player}`,
    `/DLauncherPath=${launcher}`,
    `/DUpdateHelperPath=${helper}`,
    `/DLauncherManifestPath=${launcherManifest}`,
    `/DLauncherVersionPath=${launcherVersion}`,
    `/DGameManifestPath=${gameManifest}`,
    `/DGameVersionPath=${gameVersion}`,
    "/DAppVersion=3.0.0",
    `/DOutputDir=${fixture}`,
    scriptPath,
  ]);
  installerPath = path.join(fixture, "Rebellion2-3.0.0-Setup.exe");
}

async function runInstaller(directory, name) {
  return command(installerPath, [
    "/VERYSILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/CLOSEAPPLICATIONS",
    `/DIR=${directory}`,
    `/LOG=${path.join(scratch, `installer-${name}.log`)}`,
  ]);
}

async function buildOlderLauncher(environment) {
  const directory = path.join(scratch, "older-source");
  const archive = path.join(scratch, "older-source.zip");
  await command("git", [
    "archive",
    "--format=zip",
    `--output=${archive}`,
    "v0.0.26",
    "launcher",
  ]);
  await fs.mkdir(directory);
  await command("tar.exe", ["-xf", archive, "-C", directory]);
  const main = path.join(directory, "launcher/src-tauri/src/main.rs");
  const source = await fs.readFile(main, "utf8");
  assert(
    source.includes(
      "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d",
    ),
  );
  await fs.writeFile(
    main,
    source.replaceAll(
      "cde4cdf1c2aa34dcf2484c213fe3ad28c63543aa7de6615aa7537fce968f370d",
      publicKey,
    ),
  );
  const launcherTarget = path.join(repository, "launcher/src-tauri/target");
  const helperTarget = path.join(repository, "launcher/self-update/target");
  const launcher = path.join(launcherTarget, "debug/rebellion2-launcher.exe");
  const helper = path.join(helperTarget, "debug/rebellion2-self-update.exe");
  try {
    await command(
      "cargo",
      [
        "build",
        "--manifest-path",
        path.join(directory, "launcher/src-tauri/Cargo.toml"),
        "--target-dir",
        launcherTarget,
      ],
      { env: environment },
    );
    olderLauncherBytes = await fs.readFile(launcher);
    await command("cargo", [
      "build",
      "--manifest-path",
      path.join(directory, "launcher/self-update/Cargo.toml"),
      "--target-dir",
      helperTarget,
    ]);
    olderHelperBytes = await fs.readFile(helper);
  } finally {
    await fs.writeFile(launcher, launcherBytes);
    await fs.writeFile(helper, helperBytes);
  }
  olderBuild = {
    revision: (await command("git", ["rev-parse", "v0.0.26"])).trim(),
    fixtureMain: digest(await fs.readFile(main)),
    launcher: digest(olderLauncherBytes),
    helper: digest(olderHelperBytes),
  };
}

function manifest(version, files) {
  return {
    version,
    files: [...files].map(([file, bytes]) => ({
      path: file,
      size: bytes.length,
      sha256: digest(bytes),
    })),
  };
}

function publishManifest(location, document, files) {
  const bytes = Buffer.from(JSON.stringify(document));
  state.documents.set(location, bytes);
  for (const [, body] of files) state.blobs.set(digest(body), body);
  return sign(null, bytes, privateKey).toString("hex");
}

function zipArchive(files) {
  const locals = [];
  const central = [];
  let offset = 0;
  for (const [relative, bytes] of files) {
    const name = Buffer.from(relative);
    let crc = 0xffffffff;
    for (const byte of bytes) {
      crc ^= byte;
      for (let bit = 0; bit < 8; bit++)
        crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
    }
    crc = (crc ^ 0xffffffff) >>> 0;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50);
    local.writeUInt16LE(20, 4);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(bytes.length, 18);
    local.writeUInt32LE(bytes.length, 22);
    local.writeUInt16LE(name.length, 26);
    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt32LE(crc, 16);
    entry.writeUInt32LE(bytes.length, 20);
    entry.writeUInt32LE(bytes.length, 24);
    entry.writeUInt16LE(name.length, 28);
    entry.writeUInt32LE(offset, 42);
    locals.push(local, name, bytes);
    central.push(entry, name);
    offset += local.length + name.length + bytes.length;
  }
  const entries = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50);
  end.writeUInt16LE(files.size, 8);
  end.writeUInt16LE(files.size, 10);
  end.writeUInt32LE(entries.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, entries, end]);
}

function publishPack(
  version = "1.0.0",
  id = "runtime-pack",
  directory = "RuntimePack",
) {
  const files = new Map([
    [
      "pack.xml",
      Buffer.from(
        `<ContentPack><ID>${id}</ID><Version>${version}</Version></ContentPack>`,
      ),
    ],
    ["data.bin", Buffer.from(`pack data ${version}`)],
    ["shared.bin", Buffer.from("unchanged pack asset")],
  ]);
  publishManifest(`/dist/${id}.json`, manifest(version, files), files);
  const pack = {
    id,
    name: `Runtime fixture ${id}`,
    version,
    directory,
    manifest: `dist/${id}.json`,
    blobs: "pack-blobs/",
    depends: { rebellion2: ">=2.0.0" },
  };
  state.documents.set("/dist/catalogue.json", {
    schemaVersion: 2,
    packs: [pack],
  });
  return files;
}

function publishLauncher(
  version = "1.1.0",
  launcher = launcherBytes,
  helper = helperBytes,
) {
  const files = new Map([
    ["rebellion2-launcher.exe", launcher],
    ["rebellion2-update-helper.exe", helper],
  ]);
  const signature = publishManifest(
    "/dist/launcher-manifest.json",
    manifest(version, files),
    files,
  );
  state.documents.set("/dist/launcher.json", {
    version,
    platforms: {
      windows: {
        manifest: "dist/launcher-manifest.json",
        blobs: "launcher-blobs/",
        signature,
      },
    },
  });
  return files;
}

function publishGame(version) {
  const files = new Map([
    ["Rebellion2.exe", gameBytes],
    [
      "Rebellion2_Data/runtime-settings.json",
      Buffer.from(JSON.stringify({ version })),
    ],
  ]);
  const signature = publishManifest(
    "/dist/game-manifest.json",
    manifest(version, files),
    files,
  );
  publishManifest(
    `/dist/manifest-${version}.json`,
    manifest(version, state.newFiles),
    state.newFiles,
  );
  state.contentVersion = version;
  state.documents.set("/dist/game.json", {
    version,
    platforms: {
      windows: {
        manifest: "dist/game-manifest.json",
        blobs: "game-blobs/",
        signature,
      },
    },
    content: {
      version,
      manifest: `dist/manifest-${version}.json`,
      blobs: "blobs/",
    },
  });
}

function publishLegacy(version, launcherChanged) {
  const files = new Map([
    [
      "rebellion2-launcher.exe",
      launcherChanged
        ? Buffer.concat([launcherBytes, Buffer.from([0])])
        : launcherBytes,
    ],
    ["rebellion2-update-helper.exe", helperBytes],
    ["Rebellion2.exe", gameBytes],
    [
      "Rebellion2_Data/runtime-settings.json",
      Buffer.from(JSON.stringify({ version })),
    ],
  ]);
  const signature = publishManifest(
    "/dist/application-manifest.json",
    manifest(version, files),
    files,
  );
  publishManifest(
    `/dist/manifest-${version}.json`,
    manifest(version, state.newFiles),
    state.newFiles,
  );
  state.contentVersion = version;
  state.documents.delete("/dist/game.json");
  state.documents.set("/dist/application.json", {
    version,
    manifest: "dist/application-manifest.json",
    blobs: "application-blobs/",
    signature,
    content: {
      version,
      manifest: `dist/manifest-${version}.json`,
      blobs: "blobs/",
    },
  });
  return files;
}

function resetBackend() {
  const oldFiles = new Map([
    ["catalog.xml", Buffer.from('<Catalog version="old"/>')],
    ["Application/obsolete.bin", Buffer.from("obsolete")],
  ]);
  const newFiles = new Map([
    ["catalog.xml", Buffer.from('<Catalog version="new"/>')],
    ["Application/runtime.bin", Buffer.alloc(2 * 1024 * 1024, 23)],
  ]);
  state = {
    documents: new Map(),
    blobs: new Map(),
    requests: [],
    fault: null,
    archive: null,
    oldFiles,
    newFiles,
    contentVersion: "2.0.0",
  };
  const content = manifest("2.0.0", newFiles);
  publishManifest("/dist/manifest-2.0.0.json", content, newFiles);
  const game = new Map([["Rebellion2.exe", gameBytes]]);
  const gameSignature = publishManifest(
    "/dist/game-manifest.json",
    manifest("2.0.0", game),
    game,
  );
  state.documents.set("/dist/game.json", {
    version: "2.0.0",
    platforms: {
      windows: {
        manifest: "dist/game-manifest.json",
        blobs: "game-blobs/",
        signature: gameSignature,
      },
    },
    content: {
      version: "2.0.0",
      manifest: "dist/manifest-2.0.0.json",
      blobs: "blobs/",
    },
  });
  state.documents.set("/dist/catalogue.json", { schemaVersion: 2, packs: [] });
}

async function serve(request, response) {
  const url = new URL(request.url, base);
  const record = {
    path: url.pathname,
    version: url.searchParams.get("v"),
    range: request.headers.range,
    authorization: request.headers.authorization,
  };
  state.requests.push(record);
  if (url.pathname.startsWith("/auth/")) {
    const done = `${base}auth/done?ok=1&url=${encodeURIComponent(`${base}archive.zip`)}&token=${encodeURIComponent(token)}`;
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end(
      `<html><body><a id="verify" href="${done}">Verify fixture ownership</a></body></html>`,
    );
    return;
  }
  let bytes;
  let protectedResource = false;
  if (url.pathname === "/archive.zip") bytes = state.archive;
  else if (
    /\/(?:blobs|game-blobs|launcher-blobs|application-blobs|pack-blobs)\//.test(
      url.pathname,
    )
  ) {
    bytes = state.blobs.get(url.pathname.split("/").at(-1));
    protectedResource = url.pathname.startsWith("/blobs/");
  } else {
    const value = state.documents.get(url.pathname);
    bytes =
      value &&
      (Buffer.isBuffer(value) ? value : Buffer.from(JSON.stringify(value)));
    protectedResource = url.pathname.startsWith("/dist/manifest-");
  }
  if (!bytes) {
    response.writeHead(404);
    response.end();
    return;
  }
  const fault = state.fault;
  if (fault?.pauseNotes && url.pathname === "/dist/runtime-notes.json") {
    fault.releaseNotes = () => response.end(bytes);
    return;
  }
  const isBlob =
    /\/(?:blobs|game-blobs|launcher-blobs|application-blobs|pack-blobs)\//.test(
      url.pathname,
    ) || url.pathname === "/archive.zip";
  const targeted =
    isBlob && (!fault?.targetHash || url.pathname.endsWith(fault.targetHash));
  if (fault?.stallManifest && url.pathname === "/dist/game-manifest.json")
    return;
  if (
    protectedResource &&
    request.headers.authorization !== `Bearer ${token}`
  ) {
    response.writeHead(401);
    response.end();
    return;
  }
  const faultStatus = fault?.disconnected
    ? (fault.statusAfterDisconnect ?? fault.status)
    : fault?.status;
  if (targeted && faultStatus && (!fault.contentOnly || protectedResource)) {
    response.writeHead(faultStatus);
    response.end();
    return;
  }
  if (!isBlob && fault?.metadataStatus) {
    response.writeHead(fault.metadataStatus);
    response.end();
    return;
  }
  if (fault?.pausePath === url.pathname) {
    fault.release = () => response.end(bytes);
    return;
  }
  if (
    isBlob &&
    fault?.range416 &&
    request.headers.range &&
    !fault.rangeRejected
  ) {
    fault.rangeRejected = true;
    response.writeHead(416);
    response.end();
    return;
  }
  if (isBlob && fault?.corrupt) bytes = Buffer.alloc(bytes.length, 91);
  const offset =
    isBlob && !fault?.ignoreRange && request.headers.range
      ? Number(request.headers.range.match(/^bytes=(\d+)-$/)?.[1])
      : 0;
  if (offset >= bytes.length && bytes.length) {
    response.writeHead(416);
    response.end();
    return;
  }
  const body = bytes.subarray(offset);
  const headers = {
    "Content-Length": body.length,
    ETag: '"fixture-v1"',
    Connection: "close",
  };
  if (offset)
    headers["Content-Range"] =
      `bytes ${offset}-${bytes.length - 1}/${bytes.length}`;
  response.writeHead(offset ? 206 : 200, headers);
  if (targeted && fault?.pauseBeforeBytes) {
    response.flushHeaders();
    fault.release = () => response.end(body);
    return;
  }
  if (
    targeted &&
    fault?.disconnect &&
    !fault.disconnected &&
    body.length > 65536
  ) {
    fault.disconnected = true;
    response.write(body.subarray(0, 65536));
    setTimeout(() => response.destroy(), 30);
    return;
  }
  if (isBlob && fault?.slow && body.length > 65536) {
    let sent = 0;
    const timer = setInterval(() => {
      if (response.destroyed) {
        clearInterval(timer);
        return;
      }
      const next = Math.min(sent + 65536, body.length);
      response.write(body.subarray(sent, next));
      sent = next;
      if (sent === body.length) {
        clearInterval(timer);
        response.end();
      }
    }, fault.intervalMs ?? 45);
    response.once("close", () => clearInterval(timer));
    return;
  }
  response.end(body);
}

const server = http.createServer((request, response) => {
  serve(request, response).catch((error) => {
    console.error(error);
    response.destroy(error);
  });
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
base = `http://127.0.0.1:${server.address().port}/`;

async function freePort() {
  const socket = net.createServer();
  socket.listen(0, "127.0.0.1");
  await once(socket, "listening");
  const port = socket.address().port;
  await new Promise((resolve) => socket.close(resolve));
  return port;
}

async function seedInstall(
  name,
  { installed = true, contentVersion = "1.0.0" } = {},
) {
  const directory = path.join(scratch, name);
  await fs.mkdir(directory, { recursive: true });
  await Promise.all([
    fs.writeFile(
      path.join(directory, "rebellion2-launcher.exe"),
      launcherBytes,
    ),
    fs.writeFile(
      path.join(directory, "rebellion2-update-helper.exe"),
      helperBytes,
    ),
    fs.writeFile(path.join(directory, "Rebellion2.exe"), gameBytes),
    fs.writeFile(path.join(directory, ".game-version"), "2.0.0"),
    fs.writeFile(
      path.join(directory, ".game-manifest.json"),
      JSON.stringify(
        manifest("2.0.0", new Map([["Rebellion2.exe", gameBytes]])),
      ),
    ),
    fs.writeFile(path.join(directory, ".launcher-version"), "1.0.0"),
    fs.writeFile(path.join(directory, ".session"), token),
  ]);
  if (installed) {
    const contentFiles =
      contentVersion === "1.0.0" ? state.oldFiles : state.newFiles;
    for (const [relative, body] of contentFiles) {
      const target = path.join(directory, "Content", relative);
      await fs.mkdir(path.dirname(target), { recursive: true });
      await fs.writeFile(target, body);
    }
    await fs.writeFile(
      path.join(directory, "Content", ".content-version"),
      contentVersion,
    );
    await fs.writeFile(
      path.join(directory, "Content", ".manifest.json"),
      JSON.stringify(manifest(contentVersion, contentFiles)),
    );
    const optional = path.join(
      directory,
      "Content",
      "Packs",
      "Optional",
      "keep.bin",
    );
    await fs.mkdir(path.dirname(optional), { recursive: true });
    await fs.writeFile(optional, "preserved");
  }
  return directory;
}

async function launch(directory, args = []) {
  const port = await freePort();
  const child = spawn(path.join(directory, "rebellion2-launcher.exe"), args, {
    cwd: directory,
    windowsHide: true,
    env: {
      ...process.env,
      WEBVIEW2_USER_DATA_FOLDER: path.join(scratch, `webview-${port}`),
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-allow-origins=* ${process.env.REB2_RUNTIME_BROWSER_ARGUMENTS || ""}`,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const session = {
    directory,
    child,
    port,
    output: "",
    pending: new Map(),
    serial: 0,
    socket: null,
  };
  child.stdout.on("data", (chunk) => {
    session.output += chunk;
  });
  child.stderr.on("data", (chunk) => {
    session.output += chunk;
  });
  sessions.add(session);
  await connect(session);
  return session;
}

async function seedLegacyInstall(name) {
  const directory = await seedInstall(name, { contentVersion: "2.0.0" });
  await fs.unlink(path.join(directory, ".game-version"));
  await fs.unlink(path.join(directory, ".game-manifest.json"));
  await fs.writeFile(path.join(directory, ".application-version"), "2.0.0");
  await fs.writeFile(
    path.join(directory, ".application-manifest.json"),
    JSON.stringify(
      manifest(
        "2.0.0",
        new Map([
          ["rebellion2-launcher.exe", launcherBytes],
          ["rebellion2-update-helper.exe", helperBytes],
          ["Rebellion2.exe", gameBytes],
        ]),
      ),
    ),
  );
  return directory;
}

async function connect(session) {
  const target = await until(async () => {
    const response = await fetch(`http://127.0.0.1:${session.port}/json/list`, {
      signal: AbortSignal.timeout(2000),
    });
    return (await response.json()).find(
      (target) => target.type === "page" && target.webSocketDebuggerUrl,
    );
  }, "WebView2 debugging target");
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await once(socket, "open");
  session.socket = socket;
  socket.addEventListener("message", (event) => {
    const response = JSON.parse(event.data);
    const pending = session.pending.get(response.id);
    if (pending) {
      session.pending.delete(response.id);
      response.error
        ? pending.reject(new Error(response.error.message))
        : pending.resolve(response.result);
    }
  });
  socket.addEventListener("close", () => {
    for (const pending of session.pending.values())
      pending.reject(new Error("WebView2 disconnected"));
    session.pending.clear();
  });
}

async function debuggerCommand(session, method, params) {
  const id = ++session.serial;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      session.pending.delete(id);
      reject(new Error("WebView2 command timed out"));
    }, 5000);
    session.pending.set(id, {
      resolve: (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      reject: (error) => {
        clearTimeout(timer);
        reject(error);
      },
    });
    session.socket.send(
      JSON.stringify({
        id,
        method,
        params,
      }),
    );
  });
}

async function evaluate(session, expression) {
  const response = await debuggerCommand(session, "Runtime.evaluate", {
    expression,
    returnByValue: true,
  });
  if (response.exceptionDetails)
    throw new Error(JSON.stringify(response.exceptionDetails));
  return response.result?.value;
}

async function text(session) {
  return evaluate(session, "document.body.innerText");
}
async function screen(session, phrase, timeout) {
  return until(
    async () =>
      (await text(session))?.toLowerCase().includes(phrase.toLowerCase()),
    `screen containing ${JSON.stringify(phrase)}`,
    timeout,
  );
}
async function click(session, choice) {
  const selector = `a[href*="choice=${choice}"]`;
  await until(
    () =>
      evaluate(
        session,
        `Boolean(document.querySelector(${JSON.stringify(selector)}))`,
      ),
    `button ${choice}`,
  );
  await evaluate(
    session,
    `document.querySelector(${JSON.stringify(selector)}).click(); true`,
  );
}
async function stop(session) {
  session.socket?.close();
  if (session.child.exitCode === null && session.child.signalCode === null) {
    session.child.kill();
    await once(session.child, "exit");
  }
  // A successful self-update relaunches from a detached helper. Clean up only
  // executables inside this scenario's isolated installation.
  const paths = ["rebellion2-launcher.exe", "rebellion2-update-helper.exe"]
    .map(
      (file) => `'${path.join(session.directory, file).replaceAll("'", "''")}'`,
    )
    .join(",");
  await command("powershell.exe", [
    "-NoProfile",
    "-NonInteractive",
    "-Command",
    `$targets = @(${paths}); Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -in $targets } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`,
  ]);
  sessions.delete(session);
}

async function verifyOwnership(session) {
  await until(
    () => evaluate(session, 'Boolean(document.getElementById("verify"))'),
    "fixture ownership page",
  );
  await evaluate(session, 'document.getElementById("verify").click(); true');
}

async function ready(session) {
  await until(
    async () =>
      (await readText(
        path.join(session.directory, "Content", ".content-version"),
      )) === state.contentVersion,
    "committed content version",
  );
  await screen(session, "up to date");
  for (const [relative, bytes] of state.newFiles)
    assert.equal(
      digest(
        await fs.readFile(path.join(session.directory, "Content", relative)),
      ),
      digest(bytes),
    );
  assert.equal(
    await readText(
      path.join(session.directory, "Content", "Application", "obsolete.bin"),
    ),
    null,
  );
  assert.equal(
    await readText(
      path.join(session.directory, "Content", "Packs", "Optional", "keep.bin"),
    ),
    "preserved",
  );
  assert.equal(
    await fs
      .stat(path.join(session.directory, ".download-cache", "content"))
      .then(
        () => true,
        () => false,
      ),
    false,
  );
}

async function scenario(name, run) {
  if (process.argv[2] && !name.includes(process.argv[2])) return;
  resetBackend();
  const started = Date.now();
  try {
    await run();
    results.push({ name, passed: true, milliseconds: Date.now() - started });
    console.log(`PASS ${name} (${Date.now() - started}ms)`);
  } catch (error) {
    await command("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-File",
      "scripts/fixtures/capture-runtime.ps1",
      "-ArtifactDirectory",
      scratch,
    ]).catch((captureError) =>
      console.error(
        `Native diagnostic capture failed: ${captureError.message}`,
      ),
    );
    const diagnostics = [];
    for (const session of sessions)
      diagnostics.push({
        directory: session.directory,
        port: session.port,
        exitCode: session.child.exitCode,
        screen: await text(session).catch(() => null),
        targets: await fetch(`http://127.0.0.1:${session.port}/json/list`, {
          signal: AbortSignal.timeout(2000),
        })
          .then((response) => response.json())
          .catch(() => null),
        output: session.output,
        log: await readText(path.join(session.directory, "launcher.log")),
      });
    results.push({ name, passed: false, error: error.stack, diagnostics });
    console.error(`FAIL ${name}: ${error.stack}`);
    throw error;
  } finally {
    for (const session of [...sessions]) await stop(session);
    await fs.writeFile(
      path.join(scratch, "results.json"),
      JSON.stringify(results, null, 2),
    );
  }
}

try {
  console.log(`Runtime artifacts: ${scratch}`);
  const environment = {
    ...process.env,
    REB2_AUTH_BASE_URL: `${base}auth/`,
    REB2_CONTENT_BASE_URL: base,
    REB2_CONTENT_PACK_BASE_URL: base,
    REB2_LAUNCHER_VERSION: "1.0.0",
    REB2_TEST_UPDATE_PUBKEY: publicKey,
  };
  await command(
    "cargo",
    ["build", "--manifest-path", "launcher/src-tauri/Cargo.toml"],
    { env: environment },
  );
  await command("cargo", [
    "build",
    "--manifest-path",
    "launcher/self-update/Cargo.toml",
  ]);
  await command("rustc", [
    "--edition=2021",
    "scripts/fixtures/game-stub.rs",
    "-o",
    path.join(scratch, "game-stub.exe"),
  ]);
  await command("rustc", [
    "--edition=2021",
    "scripts/fixtures/hold-file.rs",
    "-o",
    path.join(scratch, "hold-file.exe"),
  ]);
  await command("rustc", [
    "--edition=2021",
    "scripts/fixtures/legacy-launcher.rs",
    "-o",
    path.join(scratch, "legacy-launcher.exe"),
  ]);
  launcherBytes = await fs.readFile(
    path.join(
      repository,
      "launcher/src-tauri/target/debug/rebellion2-launcher.exe",
    ),
  );
  helperBytes = await fs.readFile(
    path.join(
      repository,
      "launcher/self-update/target/debug/rebellion2-self-update.exe",
    ),
  );
  gameBytes = await fs.readFile(path.join(scratch, "game-stub.exe"));
  if (!process.argv[2] || process.argv[2].includes("older launcher"))
    await buildOlderLauncher(environment);
  if (!process.argv[2] || /installer|reinstall/i.test(process.argv[2]))
    await buildInstallerFixture();
  const sourceFiles = (
    await command("git", [
      "ls-files",
      "--cached",
      "--others",
      "--exclude-standard",
    ])
  )
    .split(/\r?\n/)
    .filter((file) =>
      /^(launcher\/.*\.(rs|toml|lock|html|css|json)|packaging\/windows\/.*\.iss|scripts\/(test-launcher-runtime\.mjs|fixtures\/.*\.(rs|ps1)))$/.test(
        file,
      ),
    )
    .sort();
  const sourceHashes = Object.fromEntries(
    await Promise.all(
      sourceFiles.map(async (file) => [
        file,
        digest(await fs.readFile(path.join(repository, file))),
      ]),
    ),
  );
  await fs.writeFile(
    path.join(scratch, "build.json"),
    JSON.stringify(
      {
        commit: (await command("git", ["rev-parse", "HEAD"])).trim(),
        loopbackBackend: base,
        browserArguments: process.env.REB2_RUNTIME_BROWSER_ARGUMENTS || "",
        installerBaseline: Boolean(process.env.REB2_TEST_INSTALLER_BASELINE),
        installer: installerPath
          ? digest(await fs.readFile(installerPath))
          : null,
        node: process.version,
        rust: (await command("rustc", ["--version"])).trim(),
        sources: sourceHashes,
        binaries: {
          launcher: digest(launcherBytes),
          helper: digest(helperBytes),
          gameStub: digest(gameBytes),
        },
        olderBuild,
      },
      null,
      2,
    ),
  );

  for (const installed of [true, false]) {
    await scenario(
      `older launcher upgrades its helper and executable before game update (Content installed: ${installed})`,
      async () => {
        publishLauncher();
        publishGame("3.0.0");
        state.archive = zipArchive(state.newFiles);
        const directory = await seedInstall(`older-launcher-${installed}`, {
          installed,
          contentVersion: "2.0.0",
        });
        await fs.writeFile(
          path.join(directory, "rebellion2-launcher.exe"),
          olderLauncherBytes,
        );
        await fs.writeFile(
          path.join(directory, "rebellion2-update-helper.exe"),
          olderHelperBytes,
        );
        const old = await launch(directory);
        if (!installed) await verifyOwnership(old);
        await screen(old, "Launcher update available");
        await click(old, "launcher-update");
        await screen(old, "Game update available");
        assert.equal(
          state.requests.filter(
            (request) =>
              request.path.startsWith("/game-blobs/") ||
              request.path.startsWith("/blobs/"),
          ).length,
          0,
        );
        await stop(old);
        const current = await launch(directory);
        if (!installed) await verifyOwnership(current);
        await screen(current, "Game update available");
        assert.equal(
          digest(
            await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
          ),
          digest(launcherBytes),
        );
        assert.equal(
          digest(
            await fs.readFile(
              path.join(directory, "rebellion2-update-helper.exe"),
            ),
          ),
          digest(helperBytes),
        );
        await click(current, "game-update");
        if (installed) await ready(current);
        else {
          await verifyOwnership(current);
          await until(
            async () =>
              (await readText(path.join(directory, "game-started"))) ===
              "started",
            "new launcher archive installation",
          );
          assert.equal(
            state.requests.filter((request) =>
              request.path.startsWith("/blobs/"),
            ).length,
            0,
          );
        }
        assert.equal(
          await readText(path.join(directory, ".launcher-version")),
          "1.1.0",
        );
        assert.equal(
          await readText(path.join(directory, ".game-version")),
          "3.0.0",
        );
        assert.equal(
          await readText(path.join(directory, "Content/.content-version")),
          "3.0.0",
        );
      },
    );
  }

  for (const channel of ["game", "application"]) {
    await scenario(
      `fresh older game uses one matching archive after ${channel} update and zero content blobs`,
      async () => {
        if (channel === "game") publishGame("3.0.0");
        else publishLegacy("3.0.0", false);
        state.archive = zipArchive(state.newFiles);
        const directory =
          channel === "game"
            ? await seedInstall(`fresh-old-${channel}`, { installed: false })
            : await seedLegacyInstall(`fresh-old-${channel}`);
        if (channel === "application") {
          const content = path.resolve(directory, "Content");
          assert(content.startsWith(scratch + path.sep));
          await fs.rm(content, { recursive: true });
        }
        const session = await launch(directory);
        await verifyOwnership(session);
        await screen(
          session,
          channel === "game" ? "Game update available" : "Update available",
        );
        await click(session, `${channel}-update`);
        await verifyOwnership(session);
        await until(
          async () =>
            (await readText(path.join(directory, "game-started"))) ===
            "started",
          "archive installation after player update",
        );
        assert(
          state.requests.some(
            (request) =>
              request.path === "/auth/" && request.version === "3.0.0",
          ),
        );
        assert.equal(
          state.requests.filter((request) => request.path === "/archive.zip")
            .length,
          1,
        );
        assert.equal(
          state.requests.filter((request) => request.path.startsWith("/blobs/"))
            .length,
          0,
        );
        assert.equal(
          await readText(path.join(directory, "Content/.content-version")),
          "3.0.0",
        );
        for (const [relative, bytes] of state.newFiles)
          assert.equal(
            digest(
              await fs.readFile(path.join(directory, "Content", relative)),
            ),
            digest(bytes),
          );
      },
    );
  }

  await scenario(
    "an archive from an older release cannot be labeled as the installed game version",
    async () => {
      state.archive = zipArchive(state.oldFiles);
      const directory = await seedInstall("old-content-archive", {
        installed: false,
      });
      const session = await launch(directory);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await screen(session, "could not be verified");
      assert.equal(
        await readText(path.join(directory, "Content/.content-version")),
        null,
      );
      assert.equal(
        await readText(path.join(directory, "Content/catalog.xml")),
        null,
      );
      assert.equal(await readText(path.join(directory, "game-started")), null);
      state.archive = zipArchive(state.newFiles);
      await click(session, "install");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "matching archive retry",
      );
    },
  );

  await scenario(
    "completed verification is reused on normal startup",
    async () => {
      const directory = await seedInstall("verification-record");
      const first = await launch(directory);
      await screen(first, "Update available");
      await click(first, "update");
      await ready(first);
      const stored = await fs.readFile(
        path.join(directory, "Content/.manifest.json"),
      );
      assert.equal(
        await readText(
          path.join(directory, "Content/.verified-manifest.sha256"),
        ),
        digest(stored),
      );
      await stop(first);
      state.requests = [];
      const second = await launch(directory);
      await screen(second, "up to date");
      assert.equal(
        state.requests.filter((request) =>
          request.path.startsWith("/dist/manifest-"),
        ).length,
        0,
      );
    },
  );

  await scenario(
    "altering the saved manifest invalidates completed verification",
    async () => {
      const directory = await seedInstall("altered-verification-record");
      const first = await launch(directory);
      await screen(first, "Update available");
      await click(first, "update");
      await ready(first);
      await stop(first);
      const stored = JSON.parse(
        await readText(path.join(directory, "Content/.manifest.json")),
      );
      stored.files[0].sha256 = "0".repeat(64);
      await fs.writeFile(
        path.join(directory, "Content/.manifest.json"),
        JSON.stringify(stored),
      );
      state.requests = [];
      const second = await launch(directory);
      await screen(second, "up to date");
      assert(
        state.requests.some((request) =>
          request.path.startsWith("/dist/manifest-"),
        ),
      );
      assert.equal(
        await readText(
          path.join(directory, "Content/.verified-manifest.sha256"),
        ),
        digest(
          await fs.readFile(path.join(directory, "Content/.manifest.json")),
        ),
      );
    },
  );

  await scenario(
    "current version label cannot hide missing damaged or obsolete content",
    async () => {
      const directory = await seedInstall("damaged-current-content", {
        contentVersion: "2.0.0",
      });
      await fs.unlink(path.join(directory, "Content/Application/runtime.bin"));
      await fs.writeFile(
        path.join(directory, "Content/catalog.xml"),
        "damaged",
      );
      await fs.writeFile(
        path.join(directory, "Content/Application/obsolete.bin"),
        "obsolete",
      );
      const session = await launch(directory);
      await screen(session, "need repair");
      await click(session, "update");
      await ready(session);
      assert.equal(
        state.requests.filter((request) => request.path === "/archive.zip")
          .length,
        0,
      );
    },
  );

  await scenario(
    "an unavailable game channel cannot downgrade a newer player through the legacy channel",
    async () => {
      publishLegacy("3.0.0", false);
      publishManifest(
        "/dist/manifest-4.0.0.json",
        manifest("4.0.0", state.newFiles),
        state.newFiles,
      );
      const directory = await seedInstall("legacy-no-downgrade", {
        contentVersion: "4.0.0",
      });
      await fs.writeFile(path.join(directory, ".game-version"), "4.0.0");
      await fs.writeFile(
        path.join(directory, ".game-manifest.json"),
        JSON.stringify(
          manifest("4.0.0", new Map([["Rebellion2.exe", gameBytes]])),
        ),
      );
      await fs.writeFile(path.join(directory, ".application-version"), "2.0.0");
      const session = await launch(directory);
      await screen(session, "up to date");
      assert.equal(
        await readText(path.join(directory, ".game-version")),
        "4.0.0",
      );
      assert(
        !state.requests.some(
          (request) => request.path === "/dist/application-manifest.json",
        ),
      );
    },
  );

  if (process.env.REB2_RUNTIME_DISK_ROOT) {
    await scenario(
      "full disk preserves installed files and resumes without retrying a local write error",
      async () => {
        const volume = path.resolve(process.env.REB2_RUNTIME_DISK_ROOT);
        assert.match(volume, /^[A-Z]:\\$/i);
        assert.equal(
          await readText(path.join(volume, ".reb2-runtime-volume")),
          "reb2-runtime-only",
        );
        const directory = path.join(volume, path.basename(scratch));
        await fs.cp(await seedInstall("disk-full-source"), directory, {
          recursive: true,
        });
        const session = await launch(directory);
        await screen(session, "Update available");
        const free = Number(
          (
            await command("powershell.exe", [
              "-NoProfile",
              "-Command",
              `(Get-Volume -DriveLetter '${volume[0]}').SizeRemaining`,
            ])
          ).trim(),
        );
        assert(
          free > 4 * 1024 * 1024 && free < 1024 * 1024 * 1024,
          "expected an isolated small test volume",
        );
        const fillerPath = path.join(directory, "test-filler.bin");
        const filler = await fs.open(fillerPath, "wx");
        const buffer = Buffer.alloc(1024 * 1024, 57);
        let remaining = free - 512 * 1024;
        try {
          while (remaining > 0) {
            const { bytesWritten } = await filler.write(
              buffer.subarray(0, Math.min(buffer.length, remaining)),
            );
            remaining -= bytesWritten;
          }
          await filler.sync();
        } finally {
          await filler.close();
        }
        try {
          await click(session, "update");
          await screen(session, "free disk space");
          const bytes = state.newFiles.get("Application/runtime.bin");
          const hash = digest(bytes);
          assert.equal(
            state.requests.filter(
              (request) => request.path === `/blobs/${hash}`,
            ).length,
            1,
          );
          const partial = await fs.stat(
            path.join(directory, ".download-cache/content", `${hash}.part`),
          );
          assert(partial.size > 0 && partial.size < bytes.length);
          assert.equal(
            await readText(path.join(directory, "Content/.content-version")),
            "1.0.0",
          );
          assert.equal(
            await readText(path.join(directory, "Content/catalog.xml")),
            state.oldFiles.get("catalog.xml").toString(),
          );
          await fs.unlink(fillerPath);
          await click(session, "update");
          await ready(session);
          assert(
            state.requests.some(
              (request) => request.path === `/blobs/${hash}` && request.range,
            ),
          );
        } finally {
          await fs.rm(fillerPath, { force: true });
        }
      },
    );
  }

  for (const channel of ["game", "launcher", "application"]) {
    await scenario(
      `Update responds immediately without fetching displayed patch notes again (${channel})`,
      async () => {
        if (channel === "game") publishGame("3.0.0");
        else if (channel === "launcher") publishLauncher();
        else publishLegacy("3.0.0", false);
        const version = channel === "launcher" ? "1.1.0" : "3.0.0";
        const notes = Buffer.from(
          JSON.stringify({
            version,
            sections: [{ title: "Fixes", items: ["Runtime fixture update."] }],
          }),
        );
        state.documents.set("/dist/runtime-notes.json", notes);
        const pointer = {
          path: "dist/runtime-notes.json",
          sha256: digest(notes),
        };
        if (channel === "game")
          state.documents.get("/dist/game.json").content.releaseNotes = pointer;
        else if (channel === "launcher")
          state.documents.get("/dist/launcher.json").releaseNotes = pointer;
        else
          state.documents.get("/dist/application.json").content.releaseNotes =
            pointer;
        const directory =
          channel === "application"
            ? await seedLegacyInstall("update-click-application")
            : await seedInstall(`update-click-${channel}`, {
                contentVersion: "2.0.0",
              });
        const session = await launch(directory);
        await screen(session, "Runtime fixture update.");
        const notesRequests = state.requests.filter(
          (request) => request.path === "/dist/runtime-notes.json",
        ).length;
        state.fault = { pauseNotes: true };
        await click(session, `${channel}-update`);
        await screen(session, "Updating", 1500);
        if (channel === "launcher") await screen(session, "up to date");
        else await ready(session);
        assert.equal(
          state.requests.filter(
            (request) => request.path === "/dist/runtime-notes.json",
          ).length,
          notesRequests,
        );
      },
    );
  }

  await scenario(
    "rapid Update clicks start one operation and keep the UI responsive",
    async () => {
      publishGame("3.0.0");
      const hash = digest(Buffer.from(JSON.stringify({ version: "3.0.0" })));
      state.fault = { targetHash: hash, pauseBeforeBytes: true };
      const session = await launch(
        await seedInstall("rapid-update-clicks", { contentVersion: "2.0.0" }),
      );
      await screen(session, "Game update available");
      await evaluate(
        session,
        'const update = document.querySelector("a[href*=game-update]"); for (let i = 0; i < 8; i++) update.click(); true',
      );
      await until(() => state.fault.release, "paused player payload");
      await screen(session, "Updating");
      assert.equal(
        await evaluate(
          session,
          'document.querySelectorAll("a[href*=game-update]").length',
        ),
        0,
      );
      assert.equal(
        state.requests.filter((request) => request.path.endsWith(hash)).length,
        1,
      );
      state.fault.release();
      await ready(session);
    },
  );

  await scenario(
    "slow first install reports progress before twenty megabytes arrive",
    async () => {
      state.archive = zipArchive(state.newFiles);
      state.fault = { slow: true, intervalMs: 100 };
      const session = await launch(
        await seedInstall("slow-first-install", { installed: false }),
      );
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await until(async () => {
        const percent = await evaluate(
          session,
          'parseInt(document.getElementById("p")?.textContent || "0", 10)',
        );
        return percent > 0 && percent < 100;
      }, "intermediate slow-download progress");
      assert(
        (await fs.stat(path.join(session.directory, "content.zip.part"))).size <
          20 * 1024 * 1024,
      );
      await until(
        async () =>
          (await readText(path.join(session.directory, "game-started"))) ===
          "started",
        "launch after slow first install",
      );
    },
  );

  await scenario(
    "progress remains zero until response bytes arrive",
    async () => {
      const bytes = state.newFiles.get("Application/runtime.bin");
      state.fault = { targetHash: digest(bytes), pauseBeforeBytes: true };
      const session = await launch(await seedInstall("zero-progress"));
      await click(session, "update");
      await until(() => state.fault.release, "paused response headers");
      await until(
        () =>
          evaluate(
            session,
            'document.getElementById("f")?.style.width === "0%"',
          ),
        "zero-byte progress display",
      );
      assert.equal(
        await evaluate(session, 'document.getElementById("f").style.width'),
        "0%",
      );
      assert.equal(
        await evaluate(session, 'document.getElementById("p").textContent'),
        "",
      );
      state.fault.release();
      await ready(session);
    },
  );

  await scenario(
    "successful content update preserves optional packs",
    async () => {
      const session = await launch(await seedInstall("successful"));
      await screen(session, "Update available");
      await click(session, "update");
      await ready(session);
      const screenshot = await debuggerCommand(
        session,
        "Page.captureScreenshot",
        { format: "png" },
      );
      await fs.writeFile(
        path.join(scratch, "ready.png"),
        Buffer.from(screenshot.data, "base64"),
      );
    },
  );

  await scenario("dropped HTTP response resumes from saved bytes", async () => {
    state.fault = { disconnect: true };
    const session = await launch(await seedInstall("disconnected"));
    await screen(session, "Update available");
    await click(session, "update");
    await ready(session);
    assert(
      state.requests.some(
        (request) =>
          request.path.startsWith("/blobs/") &&
          request.range &&
          request.range !== "bytes=0-",
      ),
    );
  });

  await scenario(
    "second launcher cannot recover or mutate active staging",
    async () => {
      state.fault = { slow: true };
      const directory = await seedInstall("simultaneous");
      const first = await launch(directory);
      await screen(first, "Update available");
      await click(first, "update");
      await until(
        async () =>
          (
            await fs
              .stat(
                path.join(
                  directory,
                  ".download-cache",
                  "content",
                  `${digest(state.newFiles.get("Application/runtime.bin"))}.part`,
                ),
              )
              .catch(() => null)
          )?.size > 0,
        "active partial download",
      );
      const second = await launch(directory);
      await screen(second, "already open");
      await ready(first);
    },
  );

  await scenario(
    "terminating the launcher during download preserves resumable state",
    async () => {
      state.fault = { slow: true };
      const directory = await seedInstall("terminated");
      const first = await launch(directory);
      await screen(first, "Update available");
      await click(first, "update");
      await until(
        async () =>
          (
            await fs
              .stat(
                path.join(
                  directory,
                  ".download-cache",
                  "content",
                  `${digest(state.newFiles.get("Application/runtime.bin"))}.part`,
                ),
              )
              .catch(() => null)
          )?.size >= 131072,
        "saved partial bytes",
      );
      await stop(first);
      assert.equal(
        await readText(path.join(directory, "Content", "catalog.xml")),
        state.oldFiles.get("catalog.xml").toString(),
      );
      state.fault = null;
      const second = await launch(directory);
      await screen(second, "Update available");
      await click(second, "update");
      await ready(second);
      assert(
        state.requests.some(
          (request) => request.path.startsWith("/blobs/") && request.range,
        ),
      );
    },
  );

  await scenario(
    "corrupt downloads never change live files and retry recovers",
    async () => {
      state.fault = { corrupt: true };
      const directory = await seedInstall("corruption");
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await screen(session, "Update interrupted", 30000);
      assert.equal(
        await readText(path.join(directory, "Content", "catalog.xml")),
        state.oldFiles.get("catalog.xml").toString(),
      );
      assert.equal(
        await readText(path.join(directory, "Content", ".content-version")),
        "1.0.0",
      );
      state.fault = null;
      await click(session, "update");
      await ready(session);
    },
  );

  await scenario(
    "missing blobs expose retry and keep installed content",
    async () => {
      state.fault = { status: 404 };
      const directory = await seedInstall("missing");
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await screen(session, "Update unavailable");
      assert.equal(
        await readText(path.join(directory, "Content", ".content-version")),
        "1.0.0",
      );
      state.fault = null;
      await click(session, "update");
      await ready(session);
    },
  );

  await scenario(
    "expired authorization requests ownership without losing pending update",
    async () => {
      state.fault = { status: 401 };
      const session = await launch(await seedInstall("authorization"));
      await screen(session, "Update available");
      await click(session, "update");
      await until(
        async () =>
          (await readText(path.join(session.directory, ".session"))) === null,
        "cleared expired session",
      );
      state.fault = null;
      await click(session, "signin");
      await until(
        () => evaluate(session, 'Boolean(document.getElementById("verify"))'),
        "fixture ownership page",
      );
      await evaluate(
        session,
        'document.getElementById("verify").click(); true',
      );
      await ready(session);
    },
  );

  await scenario(
    "malformed recovery journal fails closed before update or play",
    async () => {
      const directory = await seedInstall("invalid-journal");
      const transaction = path.join(
        directory,
        "Content",
        ".rebellion2-update-transaction",
      );
      await fs.mkdir(transaction, { recursive: true });
      await fs.writeFile(path.join(transaction, "transaction.json"), "{broken");
      const session = await launch(directory);
      await screen(session, "Update recovery failed");
      assert.equal(
        await evaluate(
          session,
          "Boolean(document.querySelector('a[href*=\"choice=play\"]'))",
        ),
        false,
      );
      assert.equal(
        await readText(path.join(transaction, "transaction.json")),
        "{broken",
      );
      assert.equal(state.requests.length, 0);
    },
  );

  await scenario(
    "invalid signed game manifest cannot change the installation",
    async () => {
      const files = new Map([["../outside.exe", Buffer.from("unsafe")]]);
      const signature = publishManifest(
        "/dist/unsafe-game.json",
        manifest("3.0.0", files),
        files,
      );
      state.documents.get("/dist/game.json").version = "3.0.0";
      state.documents.get("/dist/game.json").platforms.windows = {
        manifest: "dist/unsafe-game.json",
        blobs: "game-blobs/",
        signature,
      };
      state.documents.get("/dist/game.json").content.version = "3.0.0";
      const session = await launch(await seedInstall("unsafe-manifest"));
      await screen(session, "Game update available");
      await click(session, "game-update");
      await screen(session, "Update failed");
      assert.equal(
        await readText(path.join(session.directory, ".game-version")),
        "2.0.0",
      );
      assert.equal(
        state.requests.some((request) =>
          request.path.startsWith("/game-blobs/"),
        ),
        false,
      );
    },
  );

  await scenario(
    "failed game launch after installation offers retry instead of staying busy",
    async () => {
      state.archive = zipArchive(state.newFiles);
      const directory = await seedInstall("failed-first-launch", {
        installed: false,
      });
      const session = await launch(directory);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      state.fault = { pausePath: "/dist/manifest-2.0.0.json" };
      await click(session, "install");
      await until(() => state.fault.release, "paused content verification");
      await fs.writeFile(path.join(directory, ".game-version"), "3.0.0");
      state.fault.release();
      await screen(session, "Couldn't start");
      assert.equal(await readText(path.join(directory, "game-started")), null);
      assert.equal(
        await readText(path.join(directory, "Content/.content-version")),
        "2.0.0",
      );
      await fs.writeFile(path.join(directory, ".game-version"), "2.0.0");
      await click(session, "play");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "game launch after retry",
      );
    },
  );

  await scenario(
    "first install verifies the archive and launches the real game stub",
    async () => {
      state.archive = zipArchive(state.newFiles);
      const directory = await seedInstall("first-install", {
        installed: false,
      });
      const session = await launch(directory);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "game launch after first install",
      );
      assert.equal(
        await readText(path.join(directory, "Content", ".content-version")),
        "2.0.0",
      );
      for (const [relative, bytes] of state.newFiles)
        assert.equal(
          digest(await fs.readFile(path.join(directory, "Content", relative))),
          digest(bytes),
        );
    },
  );

  await scenario(
    "corrupt repair archive preserves installed files and Retry redownloads it",
    async () => {
      state.archive = Buffer.from("not a valid ZIP");
      const directory = await seedInstall("repair");
      const session = await launch(directory, ["--repair"]);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await screen(session, "Download interrupted");
      assert.equal(
        await readText(path.join(directory, "Content", "catalog.xml")),
        state.oldFiles.get("catalog.xml").toString(),
      );
      state.archive = zipArchive(state.newFiles);
      await click(session, "install");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "game launch after repair retry",
      );
      assert.equal(
        await readText(path.join(directory, "Content", ".content-version")),
        "2.0.0",
      );
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "Optional", "keep.bin"),
        ),
        "preserved",
      );
      assert.equal(
        state.requests.filter((request) => request.path === "/archive.zip")
          .length,
        2,
      );
    },
  );

  await scenario(
    "content pack install update and remove use the real manager controls",
    async () => {
      publishPack();
      const directory = await seedInstall("pack-manager", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "up to date");
      await click(session, "content-packs");
      await click(session, "content-pack-install:runtime-pack");
      await until(
        async () =>
          JSON.parse(
            (await readText(
              path.join(
                directory,
                ".content-packs",
                "runtime-pack",
                "install.json",
              ),
            )) || "{}",
          ).version === "1.0.0",
        "installed pack metadata",
      );
      await screen(session, "Installed 1.0.0");
      await fs.unlink(
        path.join(directory, "Content", "Packs", "RuntimePack", "shared.bin"),
      );
      publishPack("1.1.0");
      await click(session, "back-to-ready");
      await click(session, "content-packs");
      await screen(session, "Update 1.1.0 available");
      await click(session, "content-pack-install:runtime-pack");
      await until(
        async () =>
          JSON.parse(
            (await readText(
              path.join(
                directory,
                ".content-packs",
                "runtime-pack",
                "install.json",
              ),
            )) || "{}",
          ).version === "1.1.0",
        "updated pack metadata",
      );
      await screen(session, "Installed 1.1.0");
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "RuntimePack", "data.bin"),
        ),
        "pack data 1.1.0",
      );
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "RuntimePack", "shared.bin"),
        ),
        "unchanged pack asset",
      );
      await click(session, "content-pack-remove:runtime-pack");
      await until(
        async () =>
          !(await fs
            .stat(path.join(directory, ".content-packs", "runtime-pack"))
            .catch(() => null)),
        "removed pack metadata",
      );
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "RuntimePack", "data.bin"),
        ),
        null,
      );
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "Optional", "keep.bin"),
        ),
        "preserved",
      );
    },
  );

  await scenario(
    "installing another pack preserves a failed pack's resumable download",
    async () => {
      const aFiles = publishPack("1.0.0", "runtime-a", "RuntimeA");
      aFiles.set("data.bin", Buffer.alloc(1024 * 1024, 29));
      publishManifest(
        "/dist/runtime-a.json",
        manifest("1.0.0", aFiles),
        aFiles,
      );
      const a = state.documents.get("/dist/catalogue.json").packs[0];
      publishPack("1.0.0", "runtime-b", "RuntimeB");
      state.documents.get("/dist/catalogue.json").packs.unshift(a);
      const hash = digest(aFiles.get("data.bin"));
      state.fault = {
        targetHash: hash,
        disconnect: true,
        statusAfterDisconnect: 503,
      };
      const directory = await seedInstall("pack-cache-isolation", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "up to date");
      await click(session, "content-packs");
      await click(session, "content-pack-install:runtime-a");
      await screen(session, "Installation failed");
      const partial = path.join(
        directory,
        ".download-cache",
        "content-packs",
        "runtime-a",
        `${hash}.part`,
      );
      const length = (await fs.stat(partial)).size;
      assert(length > 0 && length < aFiles.get("data.bin").length);
      await click(session, "content-packs");
      await click(session, "content-pack-install:runtime-b");
      await screen(session, "Installed 1.0.0");
      assert.equal((await fs.stat(partial)).size, length);
      state.fault = null;
      await click(session, "content-pack-install:runtime-a");
      await until(
        async () =>
          (
            await readText(
              path.join(directory, "Content", "Packs", "RuntimeA", "data.bin"),
            )
          )?.length === aFiles.get("data.bin").length,
        "resumed pack installed",
      );
      await screen(session, "Installed 1.0.0");
      assert(
        state.requests.some(
          (request) =>
            request.path.endsWith(hash) && request.range === `bytes=${length}-`,
        ),
      );
    },
  );

  await scenario(
    "locked pack registration rolls back both update and removal",
    async () => {
      publishPack();
      const directory = await seedInstall("locked-pack-registration", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "up to date");
      await click(session, "content-packs");
      await click(session, "content-pack-install:runtime-pack");
      await screen(session, "Installed 1.0.0");
      const registration = path.join(
        directory,
        ".content-packs",
        "runtime-pack",
        "install.json",
      );
      const asset = path.join(
        directory,
        "Content",
        "Packs",
        "RuntimePack",
        "data.bin",
      );
      for (const operation of ["update", "remove"]) {
        if (operation === "update") {
          publishPack("1.1.0");
          await click(session, "back-to-ready");
          await click(session, "content-packs");
          await screen(session, "Update 1.1.0 available");
        }
        const previousVersion = operation === "update" ? "1.0.0" : "1.1.0";
        const locker = spawn(
          path.join(scratch, "hold-file.exe"),
          [registration],
          { windowsHide: true },
        );
        await once(locker.stdout, "data");
        try {
          await click(
            session,
            `content-pack-${operation === "update" ? "install" : "remove"}:runtime-pack`,
          );
          await screen(
            session,
            operation === "update" ? "Installation failed" : "Removal failed",
          );
          assert.equal(
            JSON.parse(await readText(registration)).version,
            previousVersion,
          );
          assert.equal(await readText(asset), `pack data ${previousVersion}`);
          assert(
            await fs.stat(
              path.join(
                directory,
                ".rebellion2-update-transaction",
                "transaction.json",
              ),
            ),
          );
          locker.stdin.end();
          await once(locker, "exit");
          await click(session, "content-packs");
          await click(
            session,
            `content-pack-${operation === "update" ? "install" : "remove"}:runtime-pack`,
          );
          if (operation === "update") await screen(session, "Installed 1.1.0");
          else {
            await until(
              async () => (await readText(registration)) === null,
              "pack removed after unlocking",
            );
            assert.equal(await readText(asset), null);
          }
        } finally {
          if (locker.exitCode === null) {
            locker.kill();
            await once(locker, "exit");
          }
        }
      }
    },
  );

  await scenario(
    "invalid manifest signature is rejected before any game blob is downloaded",
    async () => {
      state.documents.get("/dist/game.json").version = "3.0.0";
      state.documents.get("/dist/game.json").content.version = "3.0.0";
      state.documents.get("/dist/game.json").platforms.windows.signature =
        "00".repeat(64);
      const session = await launch(await seedInstall("invalid-signature"));
      await screen(session, "Game update available");
      await click(session, "game-update");
      await screen(session, "Update failed");
      assert.equal(
        await readText(path.join(session.directory, ".game-version")),
        "2.0.0",
      );
      assert.equal(
        state.requests.some((request) =>
          request.path.startsWith("/game-blobs/"),
        ),
        false,
      );
    },
  );

  await scenario(
    "server ignoring Range replaces the partial instead of appending",
    async () => {
      state.fault = { ignoreRange: true };
      const directory = await seedInstall("ignored-range");
      const bytes = state.newFiles.get("Application/runtime.bin");
      const cache = path.join(directory, ".download-cache", "content");
      await fs.mkdir(cache, { recursive: true });
      await fs.writeFile(
        path.join(cache, `${digest(bytes)}.part`),
        bytes.subarray(0, 131072),
      );
      await fs.writeFile(
        path.join(cache, `${digest(bytes)}.part.size`),
        String(bytes.length),
      );
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await ready(session);
      assert(
        state.requests.some((request) => request.range === "bytes=131072-"),
      );
    },
  );

  await scenario(
    "416 resume response discards stale partial and retries a complete download",
    async () => {
      state.fault = { range416: true };
      const directory = await seedInstall("range-416");
      const bytes = state.newFiles.get("Application/runtime.bin");
      const cache = path.join(directory, ".download-cache", "content");
      await fs.mkdir(cache, { recursive: true });
      await fs.writeFile(
        path.join(cache, `${digest(bytes)}.part`),
        bytes.subarray(0, 131072),
      );
      await fs.writeFile(
        path.join(cache, `${digest(bytes)}.part.size`),
        String(bytes.length),
      );
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await ready(session);
      const requests = state.requests.filter((request) =>
        request.path.endsWith(digest(bytes)),
      );
      assert.equal(requests[0].range, "bytes=131072-");
      assert.equal(requests[1].range, undefined);
    },
  );

  await scenario(
    "unreachable release channel still permits launching installed game",
    async () => {
      state.fault = { metadataStatus: 503 };
      const directory = await seedInstall("unreachable-channel", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "still available to play");
      await click(session, "play");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "offline launch",
      );
    },
  );

  for (const relative of [
    "Application/obsolete.bin",
    ".content-version",
    ".manifest.json",
    ".verified-manifest.sha256",
  ]) {
    await scenario(
      `locked ${relative} during commit retains recovery journal and succeeds after unlocking`,
      async () => {
        const directory = await seedInstall(
          `locked-commit-${relative.replaceAll("/", "-")}`,
        );
        if (relative === ".verified-manifest.sha256")
          await fs.writeFile(
            path.join(directory, "Content", relative),
            "old record",
          );
        const locker = spawn(
          path.join(scratch, "hold-file.exe"),
          [path.join(directory, "Content", relative)],
          { windowsHide: true },
        );
        await once(locker.stdout, "data");
        try {
          const session = await launch(directory);
          await screen(session, "Update available");
          await click(session, "update");
          await screen(session, "Update interrupted");
          assert.equal(
            await readText(path.join(directory, "Content", ".content-version")),
            "1.0.0",
          );
          assert.equal(
            await readText(path.join(directory, "Content", "catalog.xml")),
            state.oldFiles.get("catalog.xml").toString(),
          );
          assert(
            await fs.stat(
              path.join(
                directory,
                "Content",
                ".rebellion2-update-transaction",
                "transaction.json",
              ),
            ),
          );
          locker.stdin.end();
          await once(locker, "exit");
          await click(session, "update");
          await ready(session);
        } finally {
          if (locker.exitCode === null) {
            locker.kill();
            await once(locker, "exit");
          }
        }
      },
    );
  }

  await scenario(
    "installer supports fresh installation and repeated same-version setup",
    async () => {
      publishGame("3.0.0");
      state.archive = zipArchive(state.newFiles);
      const directory = path.join(scratch, "installer-fresh");
      await runInstaller(directory, "fresh");
      await fs.writeFile(
        path.join(directory, ".launcher-version.pending"),
        "1.1.0",
      );
      await fs.writeFile(
        path.join(directory, ".rebellion2-launcher.next.exe"),
        "stale",
      );
      await runInstaller(directory, "repeat");
      assert.equal(
        await readText(path.join(directory, ".launcher-version.pending")),
        null,
      );
      assert.equal(
        await readText(path.join(directory, ".rebellion2-launcher.next.exe")),
        null,
      );
      assert.equal(
        await readText(path.join(directory, ".launcher-version")),
        "1.2.0",
      );
      const session = await launch(directory);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "fresh installer game launch",
      );
    },
  );

  await scenario(
    "installer refuses a file in use before replacing files and succeeds after it closes",
    async () => {
      const directory = await seedInstall("installer-file-in-use", {
        contentVersion: "2.0.0",
      });
      const locker = spawn(
        path.join(scratch, "hold-file.exe"),
        [path.join(directory, "Rebellion2.exe")],
        { windowsHide: true },
      );
      await once(locker.stdout, "data");
      try {
        await assert.rejects(runInstaller(directory, "file-in-use"));
        assert.equal(
          await readText(path.join(directory, ".launcher-version")),
          "1.0.0",
        );
        assert.equal(
          await readText(path.join(directory, ".game-version")),
          "2.0.0",
        );
      } finally {
        locker.stdin.end();
        await once(locker, "exit");
      }
      await runInstaller(directory, "file-closed");
      assert.equal(
        await readText(path.join(directory, ".launcher-version")),
        "1.2.0",
      );
      assert.equal(
        await readText(path.join(directory, ".game-version")),
        "3.0.0",
      );
    },
  );

  await scenario(
    "installer recovers interrupted changes before replacing files during reinstall",
    async () => {
      publishGame("3.0.0");
      const directory = await seedInstall("installer-recovery", {
        contentVersion: "2.0.0",
      });
      const files = new Map([
        ["Rebellion2.exe", gameBytes],
        ["rebellion2-launcher.exe", launcherBytes],
        ["rebellion2-update-helper.exe", helperBytes],
        [
          "Rebellion2_Data/runtime-settings.json",
          Buffer.from("old player settings"),
        ],
        ["Content/Packs/Optional/keep.bin", Buffer.from("preserved")],
      ]);
      const journal = path.join(directory, ".rebellion2-update-transaction");
      for (const [relative, bytes] of files) {
        const backup = path.join(journal, "backup", relative);
        await fs.mkdir(path.dirname(backup), { recursive: true });
        await fs.writeFile(backup, bytes);
        await fs.mkdir(path.dirname(path.join(directory, relative)), {
          recursive: true,
        });
        await fs.writeFile(
          path.join(directory, relative),
          "interrupted changes",
        );
      }
      await fs.writeFile(
        path.join(journal, "transaction.json"),
        JSON.stringify({
          plan: { changed: manifest("3.0.0", files).files, removed: [] },
          original_files: [...files.keys()],
        }),
      );
      const plugin = path.join(
        directory,
        "Rebellion2_Data/Plugins/x86_64/obsolete.dll",
      );
      await fs.mkdir(path.dirname(plugin), { recursive: true });
      await fs.writeFile(plugin, "obsolete");
      await runInstaller(directory, "recovery");
      assert.equal(
        await readText(
          path.join(directory, "Rebellion2_Data/runtime-settings.json"),
        ),
        installedSettings.toString(),
      );
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await ready(session);
      assert.equal(
        await readText(
          path.join(directory, "Rebellion2_Data/runtime-settings.json"),
        ),
        installedSettings.toString(),
      );
      assert.equal(
        await readText(path.join(directory, "Content/Packs/Optional/keep.bin")),
        "preserved",
      );
      assert.equal(await readText(plugin), null);
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(installedLauncherBytes),
      );
    },
  );

  await scenario(
    "installer refuses invalid recovery state before deleting or replacing files",
    async () => {
      const directory = await seedInstall("installer-invalid", {
        contentVersion: "2.0.0",
      });
      const journal = path.join(directory, ".rebellion2-update-transaction");
      await fs.mkdir(journal, { recursive: true });
      await fs.writeFile(path.join(journal, "transaction.json"), "invalid");
      const plugin = path.join(
        directory,
        "Rebellion2_Data/Plugins/x86_64/preserve.dll",
      );
      await fs.mkdir(path.dirname(plugin), { recursive: true });
      await fs.writeFile(plugin, "preserve");
      await assert.rejects(runInstaller(directory, "invalid"));
      assert.equal(await readText(plugin), "preserve");
      assert.equal(
        await readText(path.join(directory, ".game-version")),
        "2.0.0",
      );
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(launcherBytes),
      );
    },
  );

  await scenario(
    "installer holds the installation lock until copying completes",
    async () => {
      publishGame("3.0.0");
      const directory = await seedInstall("installer-lock", {
        contentVersion: "2.0.0",
      });
      await fs.writeFile(
        path.join(directory, ".test-installer-pause"),
        "pause",
      );
      const installation = runInstaller(directory, "lock").catch(
        (error) => error,
      );
      try {
        await until(
          () => readText(path.join(directory, ".test-installer-held")),
          "installer installation lock",
        );
        const concurrent = await launch(directory);
        await screen(concurrent, "already open");
        await stop(concurrent);
      } finally {
        await fs.writeFile(
          path.join(directory, ".test-installer-release"),
          "release",
        );
        const result = await installation;
        if (result instanceof Error) throw result;
      }
      const session = await launch(directory);
      await screen(session, "Update available");
      await click(session, "update");
      await ready(session);
    },
  );

  for (const [stagedVersion, validBaseline] of [
    ["1.1.0", true],
    ["1.2.0", true],
    ["1.1.0", false],
  ]) {
    await scenario(
      `fresh installer baseline handles staged launcher ${stagedVersion} with valid baseline ${validBaseline}`,
      async () => {
        const directory = await seedInstall("superseded-launcher-stage", {
          contentVersion: "2.0.0",
        });
        const fresh = Buffer.concat([
          launcherBytes,
          Buffer.from("fresh-installer"),
        ]);
        const stale = Buffer.concat([
          launcherBytes,
          Buffer.from("stale-update"),
        ]);
        const files = (bytes) =>
          new Map([
            ["rebellion2-launcher.exe", bytes],
            ["rebellion2-update-helper.exe", helperBytes],
          ]);
        await fs.writeFile(
          path.join(directory, "rebellion2-launcher.exe"),
          fresh,
        );
        await fs.writeFile(path.join(directory, ".launcher-version"), "1.2.0");
        await fs.writeFile(
          path.join(directory, ".launcher-manifest.json"),
          JSON.stringify(
            manifest("1.2.0", files(validBaseline ? fresh : stale)),
          ),
        );
        await fs.writeFile(
          path.join(directory, ".rebellion2-launcher.next.exe"),
          stale,
        );
        await fs.writeFile(
          path.join(directory, ".launcher-version.pending"),
          stagedVersion,
        );
        await fs.writeFile(
          path.join(directory, ".launcher-manifest.pending.json"),
          JSON.stringify(manifest(stagedVersion, files(stale))),
        );
        const session = await launch(directory);
        await screen(session, "up to date");
        assert.equal(
          await readText(path.join(directory, ".launcher-version")),
          validBaseline ? "1.2.0" : stagedVersion,
        );
        assert.equal(
          digest(
            await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
          ),
          digest(validBaseline ? fresh : stale),
        );
        assert.equal(
          await readText(path.join(directory, ".launcher-version.pending")),
          null,
        );
      },
    );
  }

  await scenario(
    "launcher handoff replaces executable promotes metadata and relaunches",
    async () => {
      const nextLauncher = Buffer.concat([
        launcherBytes,
        Buffer.from("updated launcher"),
      ]);
      const nextHelper = Buffer.concat([
        helperBytes,
        Buffer.from("updated helper"),
      ]);
      const files = new Map([
        ["rebellion2-launcher.exe", nextLauncher],
        ["rebellion2-update-helper.exe", nextHelper],
      ]);
      const signature = publishManifest(
        "/dist/launcher-manifest.json",
        manifest("1.1.0", files),
        files,
      );
      state.documents.set("/dist/launcher.json", {
        version: "1.1.0",
        platforms: {
          windows: {
            manifest: "dist/launcher-manifest.json",
            blobs: "launcher-blobs/",
            signature,
          },
        },
      });
      const directory = await seedInstall("launcher-handoff", {
        contentVersion: "2.0.0",
      });
      const first = await launch(directory);
      await screen(first, "Launcher update available");
      await click(first, "launcher-update");
      await screen(first, "up to date");
      assert.equal(
        await readText(path.join(directory, ".launcher-version")),
        "1.0.0",
      );
      assert.equal(
        await readText(path.join(directory, ".launcher-version.pending")),
        "1.1.0",
      );
      await stop(first);
      const second = await launch(directory);
      await screen(second, "up to date");
      assert.equal(
        await readText(path.join(directory, ".launcher-version")),
        "1.1.0",
      );
      assert.equal(
        await readText(path.join(directory, ".launcher-version.pending")),
        null,
      );
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(nextLauncher),
      );
      assert.equal(
        digest(
          await fs.readFile(
            path.join(directory, "rebellion2-update-helper.exe"),
          ),
        ),
        digest(nextHelper),
      );
    },
  );

  await scenario(
    "archive traversal is rejected without writing outside staging",
    async () => {
      state.archive = zipArchive(
        new Map([...state.newFiles, ["../escaped.bin", Buffer.from("escape")]]),
      );
      const directory = await seedInstall("archive-traversal", {
        installed: false,
      });
      const session = await launch(directory);
      await verifyOwnership(session);
      await screen(session, "Ready to install");
      await click(session, "install");
      await screen(session, "Download interrupted");
      assert.equal(await readText(path.join(directory, "escaped.bin")), null);
      assert.equal(
        await readText(path.join(directory, "Content", "catalog.xml")),
        null,
      );
    },
  );

  await scenario(
    "mismatched game and content cannot be launched while ownership is renewed",
    async () => {
      const directory = await seedInstall("incomplete-release");
      await fs.unlink(path.join(directory, ".session"));
      const session = await launch(directory);
      await screen(session, "Sign in required");
      assert.equal(
        await evaluate(
          session,
          "Boolean(document.querySelector('a[href*=\"choice=play\"]'))",
        ),
        false,
      );
      assert.equal(await readText(path.join(directory, "game-started")), null);
    },
  );

  for (const [tracked, combined] of [
    [false, false],
    [true, false],
    [false, true],
    [true, true],
  ]) {
    await scenario(
      `locked ${tracked ? "inventoried" : "unlisted"} native plugin rolls back player files and version during ${combined ? "combined" : "game"} update`,
      async () => {
        if (combined) publishLegacy("3.0.0", false);
        else publishGame("3.0.0");
        const directory = combined
          ? await seedLegacyInstall(`locked-native-${tracked}-combined`)
          : await seedInstall(`locked-native-${tracked}`, {
              contentVersion: "2.0.0",
            });
        const inventoryPath = path.join(
          directory,
          combined ? ".application-manifest.json" : ".game-manifest.json",
        );
        const versionPath = path.join(
          directory,
          combined ? ".application-version" : ".game-version",
        );
        const choice = combined ? "application-update" : "game-update";
        const pluginPath = "Rebellion2_Data/Plugins/x86_64/obsolete.dll";
        const plugin = path.join(directory, pluginPath);
        const settings = path.join(
          directory,
          "Rebellion2_Data",
          "runtime-settings.json",
        );
        await fs.mkdir(path.dirname(plugin), { recursive: true });
        await fs.writeFile(plugin, "old native plugin");
        await fs.writeFile(settings, "old player settings");
        if (tracked) {
          const inventory = JSON.parse(await readText(inventoryPath));
          inventory.files.push(
            manifest(
              "2.0.0",
              new Map([[pluginPath, Buffer.from("old native plugin")]]),
            ).files[0],
          );
          await fs.writeFile(inventoryPath, JSON.stringify(inventory));
        }
        const locker = spawn(path.join(scratch, "hold-file.exe"), [plugin], {
          windowsHide: true,
        });
        await once(locker.stdout, "data");
        try {
          const session = await launch(directory);
          await click(session, choice);
          await screen(session, "Update failed");
          assert.equal(await readText(versionPath), "2.0.0");
          assert.equal(await readText(settings), "old player settings");
          assert.equal(await readText(plugin), "old native plugin");
          assert(
            await fs.stat(
              path.join(
                directory,
                ".rebellion2-update-transaction",
                "transaction.json",
              ),
            ),
          );
          locker.stdin.end();
          await once(locker, "exit");
          await click(session, choice);
          await ready(session);
          assert.equal(await readText(plugin), null);
          assert.equal(JSON.parse(await readText(settings)).version, "3.0.0");
        } finally {
          if (locker.exitCode === null) {
            locker.kill();
            await once(locker, "exit");
          }
        }
      },
    );
  }

  await scenario(
    "signed game update commits player and matching content before enabling play",
    async () => {
      publishGame("3.0.0");
      const directory = await seedInstall("signed-game", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "Game update available");
      await click(session, "game-update");
      await ready(session);
      assert.equal(
        await readText(path.join(directory, ".game-version")),
        "3.0.0",
      );
      assert.equal(
        await readText(path.join(directory, ".approved-content-update")),
        null,
      );
      assert.equal(
        JSON.parse(
          await readText(
            path.join(directory, "Rebellion2_Data", "runtime-settings.json"),
          ),
        ).version,
        "3.0.0",
      );
    },
  );

  await scenario(
    "restarting between game and content update preserves approval and blocks mismatched play",
    async () => {
      publishGame("3.0.0");
      state.fault = { status: 401, contentOnly: true };
      const directory = await seedInstall("game-content-restart", {
        contentVersion: "2.0.0",
      });
      state.newFiles.set(
        "Application/next-release.bin",
        Buffer.from("new release content"),
      );
      publishGame("3.0.0");
      const first = await launch(directory);
      await screen(first, "Game update available");
      await click(first, "game-update");
      await until(
        async () =>
          (await readText(path.join(directory, ".game-version"))) === "3.0.0",
        "committed player update",
      );
      await screen(first, "Sign in required");
      assert.equal(
        await evaluate(
          first,
          "Boolean(document.querySelector('a[href*=\"choice=play\"]'))",
        ),
        false,
      );
      assert.equal(
        await readText(path.join(directory, ".approved-content-update")),
        "3.0.0",
      );
      await stop(first);
      state.fault = null;
      const second = await launch(directory);
      await screen(second, "Sign in required");
      assert.equal(
        await evaluate(
          second,
          "Boolean(document.querySelector('a[href*=\"choice=play\"]'))",
        ),
        false,
      );
      await click(second, "signin");
      await verifyOwnership(second);
      await ready(second);
      assert.equal(
        await readText(path.join(directory, ".approved-content-update")),
        null,
      );
    },
  );

  await scenario(
    "untrusted local game inventory cannot remove content owned by another layer",
    async () => {
      publishGame("3.0.0");
      const directory = await seedInstall("poisoned-game-baseline", {
        contentVersion: "2.0.0",
      });
      await fs.writeFile(
        path.join(directory, ".game-manifest.json"),
        JSON.stringify(
          manifest(
            "2.0.0",
            new Map([
              ["Rebellion2.exe", gameBytes],
              ["Content/Packs/Optional/keep.bin", Buffer.from("preserved")],
            ]),
          ),
        ),
      );
      const session = await launch(directory);
      await screen(session, "Game update available");
      await click(session, "game-update");
      await ready(session);
      assert.equal(
        await readText(
          path.join(directory, "Content", "Packs", "Optional", "keep.bin"),
        ),
        "preserved",
      );
    },
  );

  await scenario(
    "stalled signed manifest times out and leaves the installed player untouched",
    async () => {
      publishGame("3.0.0");
      state.fault = { stallManifest: true };
      const directory = await seedInstall("stalled-manifest", {
        contentVersion: "2.0.0",
      });
      const session = await launch(directory);
      await screen(session, "Game update available");
      await click(session, "game-update");
      await screen(session, "Update failed", 35000);
      assert.equal(
        await readText(path.join(directory, ".game-version")),
        "2.0.0",
      );
      assert.equal(
        await readText(path.join(directory, ".approved-content-update")),
        null,
      );
      state.fault = null;
      await click(session, "game-update");
      await ready(session);
    },
  );

  await scenario(
    "terminating midway through commit restores all original files on next startup",
    async () => {
      for (let index = 0; index < 512; index++) {
        const name = `Application/bulk-${String(index).padStart(4, "0")}.bin`;
        state.oldFiles.set(name, Buffer.from(`old-${index}`));
        state.newFiles.set(name, Buffer.from(`new-${index}`));
      }
      publishManifest(
        "/dist/manifest-2.0.0.json",
        manifest("2.0.0", state.newFiles),
        state.newFiles,
      );
      const directory = await seedInstall("killed-during-commit");
      const first = await launch(directory);
      await screen(first, "Update available");
      let killed = false;
      const observer = watch(
        path.join(directory, "Content"),
        async (_, filename) => {
          if (filename?.toString() !== "catalog.xml" || killed) return;
          if (
            (await readText(path.join(directory, "Content", "catalog.xml"))) ===
            state.newFiles.get("catalog.xml").toString()
          ) {
            killed = true;
            first.child.kill();
          }
        },
      );
      try {
        await click(first, "update");
        await until(
          () => killed,
          "termination after the first live file was replaced",
          40000,
        );
      } finally {
        observer.close();
      }
      await stop(first);
      assert.equal(
        await readText(path.join(directory, "Content", ".content-version")),
        "1.0.0",
      );
      assert(
        await fs.stat(
          path.join(
            directory,
            "Content",
            ".rebellion2-update-transaction",
            "transaction.json",
          ),
        ),
      );
      state.fault = { metadataStatus: 503 };
      const second = await launch(directory);
      await screen(second, "Check your connection");
      for (const [relative, bytes] of state.oldFiles)
        assert.equal(
          digest(await fs.readFile(path.join(directory, "Content", relative))),
          digest(bytes),
        );
      assert.equal(
        await fs
          .stat(
            path.join(directory, "Content", ".rebellion2-update-transaction"),
          )
          .then(
            () => true,
            () => false,
          ),
        false,
      );
      state.fault = null;
      await click(second, "retry-update-check");
      await screen(second, "Update available");
      await click(second, "update");
      await ready(second);
    },
  );

  await scenario(
    "combined update with unchanged launcher enables matched game before metadata promotion",
    async () => {
      publishLegacy("3.0.0", false);
      const directory = await seedLegacyInstall("combined-no-launcher-change");
      const session = await launch(directory);
      await click(session, "application-update");
      await ready(session);
      assert.equal(
        await readText(path.join(directory, ".application-version.pending")),
        "3.0.0",
      );
      assert.equal(
        await fs
          .stat(path.join(directory, ".rebellion2-launcher.next.exe"))
          .then(
            () => true,
            () => false,
          ),
        false,
      );
      await click(session, "play");
      await until(
        async () =>
          (await readText(path.join(directory, "game-started"))) === "started",
        "matched game launch after combined update",
      );
    },
  );

  await scenario(
    "combined update stages changed launcher and completes its real helper handoff",
    async () => {
      const files = publishLegacy("3.0.0", true);
      const directory = await seedLegacyInstall("combined-launcher-change");
      const first = await launch(directory);
      await click(first, "application-update");
      await ready(first);
      assert.equal(
        await readText(path.join(directory, ".application-version")),
        "2.0.0",
      );
      assert.equal(
        digest(
          await fs.readFile(
            path.join(directory, ".rebellion2-launcher.next.exe"),
          ),
        ),
        digest(files.get("rebellion2-launcher.exe")),
      );
      await stop(first);
      const second = await launch(directory);
      await screen(second, "up to date");
      assert.equal(
        await readText(path.join(directory, ".application-version")),
        "3.0.0",
      );
      assert.equal(
        await readText(path.join(directory, ".application-version.pending")),
        null,
      );
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(files.get("rebellion2-launcher.exe")),
      );
    },
  );

  await scenario(
    "legacy helper waits for the caller when no process ID is supplied",
    async () => {
      const directory = await seedInstall("legacy-helper-caller");
      const fixture = await fs.readFile(
        path.join(scratch, "legacy-launcher.exe"),
      );
      await fs.writeFile(
        path.join(directory, "rebellion2-launcher.exe"),
        fixture,
      );
      await fs.writeFile(
        path.join(directory, ".rebellion2-launcher.next.exe"),
        launcherBytes,
      );
      await fs.writeFile(
        path.join(directory, ".launcher-version.pending"),
        "1.1.0",
      );
      const parent = spawn(
        path.join(directory, "rebellion2-launcher.exe"),
        [],
        { cwd: directory, windowsHide: true },
      );
      const session = { directory, child: parent, socket: null };
      sessions.add(session);
      await once(parent.stdout, "data");
      await new Promise((resolve) => setTimeout(resolve, 700));
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(fixture),
      );
      assert.equal(
        await readText(path.join(directory, ".launcher-version")),
        "1.0.0",
      );
      parent.stdin.end();
      await once(parent, "exit");
      await until(
        async () =>
          (await readText(path.join(directory, ".launcher-version"))) ===
          "1.1.0",
        "legacy helper promotion after caller exits",
      );
      assert.equal(
        digest(
          await fs.readFile(path.join(directory, "rebellion2-launcher.exe")),
        ),
        digest(launcherBytes),
      );
    },
  );

  if (results.length === 0)
    throw new Error("No native runtime scenarios matched the filter.");
  console.log(
    `All ${results.length} native runtime scenarios passed. Report: ${path.join(scratch, "results.json")}`,
  );
} finally {
  for (const session of [...sessions]) await stop(session);
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
