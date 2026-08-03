# Build & release pipeline

How the installers in this repo are produced. For players, see the [README](../README.md).

A single GitHub Actions workflow (`build-installers.yml`) builds an **asset-free** game
player from [`rebellion2`](https://github.com/davidadas/rebellion2), builds the launcher from
[`rebellion2-infrastructure`](https://github.com/davidadas/rebellion2-infrastructure),
packages the two into a per-user Windows installer, and publishes it to a GitHub Release in
this repo. The installer ships **no art** — the launcher downloads `content.zip` from R2
after ownership verification.

Only the **Windows** leg is live today. macOS and Linux are parked in a commented block at
the bottom of the workflow until the Windows path is validated end to end.

| Platform | Package | Tool | Status |
|----------|---------|------|--------|
| Windows  | `Rebellion2-<version>-Setup.exe`       | Inno Setup (`ISCC.exe`) | live   |
| macOS    | `Rebellion2-<version>-macOS.zip`       | `zip` of the `.app`     | parked |
| Linux    | `Rebellion2-<version>-x86_64.AppImage` | `appimagetool`          | parked |

Everything is **unsigned** for now, which is why Windows and macOS warn on first launch.
Code signing (Authenticode on Windows, notarization on macOS) is on the roadmap.

## Triggering a build

The workflow has two entry points, and the difference matters:

- **Tag push — cuts a Release.** Push a tag matching `v*` to *this* repo:
  ```bash
  git tag v0.1.0
  git push origin v0.1.0
  ```
  This runs the full pipeline and publishes a GitHub Release named after the tag. A tag points
  at a commit (conventionally the tip of `main`), not a branch. Note that a tag build pulls the
  **game from `rebellion2@master`** and the **launcher from `rebellion2-infrastructure@main`** —
  the tag controls the version string and the packaging in *this* repo, not which game revision
  is built. To ship specific game/launcher code, merge it to those defaults first, then tag.

- **Manual dispatch — test build, NO Release.** Actions tab → **Build Installers** →
  **Run workflow**. The `release` job is gated on `github.ref_type == 'tag'`, so dispatch runs
  produce downloadable **artifacts** only; nothing is published to the Releases page. Inputs
  (all optional):
  - **version** — version string (blank = `0.0.0-dev`).
  - **source_ref** — `rebellion2` ref to build (default `master`).
  - **launcher_ref** — `rebellion2-infrastructure` ref to build (default `main`).

## Jobs

1. **prepare** — resolves the version: the tag name minus its `v`, else the `version` input,
   else `0.0.0-dev`.
2. **player** (`ubuntu-latest`) — checks out the game and `rebellion2-media`, pulls media LFS
   from R2, and installs it into `Assets/Content` + `Assets/Art/Models/MainMenu` for prefab
   authoring. It then builds `StandaloneWindows64` via `StandalonePlayerBuild.Build`, which
   strips `Assets/Content` and verifies it did not leak — so the shipped player stays
   asset-free.
3. **windows-installer** (`windows-latest`) — builds the launcher (`cargo build --release`),
   stamps the game `.exe` icon (`rcedit`), and packages
   `packaging/windows/rebellion2-launcher.iss` with Inno Setup.
4. **release** (`if: github.ref_type == 'tag'`) — downloads the installer artifacts and runs
   `gh release create <tag>`.

## Required secrets and variables

Set these under **Settings → Secrets and variables → Actions** before the first run:

| Secret | Purpose |
|--------|---------|
| `SOURCE_REPO_TOKEN` | PAT with **read** access to `rebellion2` and `rebellion2-infrastructure` (contents). |
| `REBELLION2_MEDIA_SSH_KEY` | Deploy key with read access to `rebellion2-media` (git checkout; mirrors the game CI). |
| `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` | R2 credentials for pulling media LFS through the proxy. |
| `UNITY_EMAIL` / `UNITY_PASSWORD` / `UNITY_LICENSE` | Unity license activation (same values as the `rebellion2` CI). |

| Variable | Purpose |
|----------|---------|
| `R2_ENDPOINT` | R2 S3 endpoint host, used to build the LFS proxy URL. |
| `R2_BUCKET`   | R2 bucket holding the media LFS objects. |

`GITHUB_TOKEN` is provided automatically and is what publishes the Release.

## Layout

```
.github/workflows/build-installers.yml     # the pipeline
packaging/windows/rebellion2-launcher.iss  # Windows (Inno Setup) installer — the live one
packaging/windows/rebellion2.nsi           # legacy NSIS script, unused by the current pipeline
packaging/linux/build-appimage.sh          # AppDir assembly + appimagetool (parked)
packaging/linux/rebellion2.desktop         # AppImage desktop entry (parked)
packaging/macos/build-zip.sh               # .app -> zip (parked)
```
