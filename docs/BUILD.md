# Build & release pipeline

How the installers in this repo are produced. For players, see the [README](../README.md).

Separate GitHub Actions workflows build **asset-free** game players from
[`rebellion2`](https://github.com/davidadas/rebellion2) and package the launcher in this
repository. OS-specific tags select which platform to build. The Windows workflow publishes
the shared update channel; the macOS workflow only attaches its archive to an existing release,
keeping the expensive macOS runner opt-in. The packages ship **no art** — the launcher downloads
content from R2 after ownership verification.

The **Windows** and **macOS** legs are live. Linux remains parked.

| Platform | Package                                | Tool                     | Status |
| -------- | -------------------------------------- | ------------------------ | ------ |
| Windows  | `Rebellion2-Windows-Setup.exe`         | Inno Setup (`ISCC.exe`)  | live   |
| macOS    | `Rebellion2-macOS.zip`                 | universal `.app` archive | live   |
| Linux    | `Rebellion2-<version>-x86_64.AppImage` | `appimagetool`           | parked |

The packages do not have platform signatures yet, which is why Windows and macOS warn on first
launch. The release workflow does apply an Ed25519 signature used only by the launcher to verify
automatic updates. Authenticode on Windows and notarization on macOS remain on the roadmap.

## Triggering a build

The workflows have three entry points, and the difference matters:

- **Windows tag — cuts a Release.** Push a tag matching `windows-*` to _this_ repo:

  ```bash
  git tag windows-0.1.0
  git push origin windows-0.1.0
  ```

  This runs the Windows pipeline and publishes GitHub Release `v0.1.0`. Tag builds check out
  `v0.1.0` from both `rebellion2` and `rebellion2-media`, so those repositories must carry the
  ordinary matching version tag before the installer tag is pushed.

- **Manual Windows dispatch — test build only.** Actions tab → **Build Windows Installer** →
  **Run workflow**. The publishing jobs are gated on `github.ref_type == 'tag'`, so dispatch runs
  produce downloadable **artifacts** without publishing a GitHub Release or changing the live
  content/update channel. Inputs
  (all optional):
  - **version** — version string (blank = `0.0.0-dev`).
  - **source_ref** — `rebellion2` ref to build (default `master`).

- **macOS tag — release attachment.** After the Windows release succeeds, push the corresponding
  platform tag:

  ```bash
  git tag macos-0.1.0
  git push origin macos-0.1.0
  ```

  The workflow verifies that the release and live content version match before starting either
  expensive build. It then attaches `Rebellion2-macOS.zip` to that release without changing the
  R2 release pointer or creating another release.

## Jobs

1. **prepare** — resolves the version: the tag name minus its `windows-` prefix, else the
   `version` input, else `0.0.0-dev`. The workflow passes this value to the Unity player,
   launcher, content package, and installer so releases do not require a source-controlled
   version bump.
2. **player-windows** (`ubuntu-latest`) — calls the shared player workflow, checks out the game
   and `rebellion2-media`, pulls media LFS from R2, and installs it into `Assets/Content` plus
   `Assets/Art/Models/MainMenu` for prefab authoring. It builds `StandaloneWindows64` via
   `StandalonePlayerBuild.Build`, which strips `Assets/Content` and verifies it did not leak.
3. **publish-content** (`ubuntu-latest`, tag builds only) — packages and uploads the immutable
   versioned content archive, manifest, and blobs. It stages `latest.json` as a workflow artifact
   but does not change the live channel.
4. **windows-installer** (`windows-latest`) — starts as soon as `player-windows` finishes, builds
   the launcher (`cargo build --release`),
   stamps the game `.exe` icon (`rcedit`), packages the one-time setup executable, and produces
   the manifest, blobs, and installed handoff helper used for later incremental application updates.
5. **release** (`if: github.ref_type == 'tag'`) — waits for the Windows installer and immutable
   content upload, creates the GitHub Release, and uploads the signed application layer. It embeds
   the matching content pointer inside `application.json`, publishes that single release pointer,
   verifies both versions through the direct and public channel, and restores the previous pointer
   if publication fails. Legacy `latest.json` remains pinned for older launchers.

The opt-in macOS workflow has its own cheap preflight and Ubuntu Unity-player job. Only its final
packaging job uses `macos-latest`; it builds the universal Tauri launcher, embeds the Unity player,
verifies the archive, and attaches it to the existing release.

## Required secrets and variables

Set these under **Settings → Secrets and variables → Actions** before the first run:

| Secret                                             | Purpose                                                                                       |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `SOURCE_REPO_TOKEN`                                | PAT with **read** access to `rebellion2` and `rebellion2-infrastructure` (contents).          |
| `REBELLION2_MEDIA_SSH_KEY`                         | Deploy key with read access to `rebellion2-media` (git checkout; mirrors the game CI).        |
| `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY`        | R2 credentials for pulling media LFS through the proxy.                                       |
| `REB2_CONTENT_BASE_URL`                            | Public content Worker base URL used for content and signed application updates.               |
| `LAUNCHER_SIGNING_KEY`                             | Ed25519 seed used to sign application manifests consumed by the launcher's automatic updater. |
| `UNITY_EMAIL` / `UNITY_PASSWORD` / `UNITY_LICENSE` | Unity license activation (same values as the `rebellion2` CI).                                |

| Variable      | Purpose                                               |
| ------------- | ----------------------------------------------------- |
| `R2_ENDPOINT` | R2 S3 endpoint host, used to build the LFS proxy URL. |
| `R2_BUCKET`   | R2 bucket holding the media LFS objects.              |

`GITHUB_TOKEN` is provided automatically and is what publishes the Release.

## Layout

```
.github/workflows/build-windows-installer.yml # windows-* release and live channel
.github/workflows/build-macos-installer.yml   # macos-* opt-in release attachment
.github/workflows/build-player.yml            # shared Ubuntu Unity player build
launcher/                                  # Tauri launcher/patcher source
launcher/self-update/                      # staged-launcher handoff helper
packaging/windows/rebellion2-launcher.iss  # Windows (Inno Setup) installer — the live one
packaging/windows/rebellion2.nsi           # legacy NSIS script, unused by the current pipeline
packaging/linux/build-appimage.sh          # AppDir assembly + appimagetool (parked)
packaging/linux/rebellion2.desktop         # AppImage desktop entry (parked)
packaging/macos/build-zip.sh               # launcher + Unity player -> one macOS app archive
```
