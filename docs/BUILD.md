# Build & release pipeline

How the installers in this repo are produced. For players, see the [README](../README.md).

A single manually-triggered GitHub Actions workflow pulls the game source
([`rebellion2`](https://github.com/davidadas/rebellion2)) and art
([`rebellion2-media`](https://github.com/davidadas/rebellion2-media)), cross-builds all
three desktop players on one Linux runner, packages each into an installer, and publishes
them to a GitHub Release in this repo.

| Platform | Package | Tool |
|----------|---------|------|
| Windows  | `Rebellion2-<version>-Setup.exe`      | NSIS (`apt nsis`) |
| Linux    | `Rebellion2-<version>-x86_64.AppImage`| `appimagetool` |
| macOS    | `Rebellion2-<version>-macOS.zip`      | `zip` of the `.app` |

Everything is **unsigned** for now. That is what lets the whole matrix run on a single
`ubuntu-latest` runner (a signed/notarized `.dmg` would require a macOS runner + `hdiutil`).
When code signing is added, the macOS leg moves to a `macos` runner and the Windows leg
gains an Authenticode step.

## Running it

Actions tab → **Build Installers** → **Run workflow**. Inputs (all optional):

- **version** — installer version string. Blank reads `bundleVersion` from the game's `ProjectSettings.asset`.
- **source_ref** — `rebellion2` ref to build (default `master`).
- **media_ref** — `rebellion2-media` ref to bundle (default: the revision pinned in the workflow).

Each run overwrites the rolling **`latest`** release and, if it does not already exist,
creates an immutable **`v<version>`** release.

## Required secrets

Set these under **Settings → Secrets and variables → Actions** before the first run:

| Secret | Purpose |
|--------|---------|
| `SOURCE_REPO_TOKEN` | PAT with **read** access to `rebellion2` **and** `rebellion2-media` (contents + LFS). A fine-grained PAT scoped to those two repos is ideal. |
| `UNITY_EMAIL`       | Unity account email (for license activation). |
| `UNITY_PASSWORD`    | Unity account password. |
| `UNITY_LICENSE`     | Contents of the Unity `.ulf` personal license file (same value used by the `rebellion2` CI). |

`GITHUB_TOKEN` is provided automatically and is what publishes the releases.

## Layout

```
.github/workflows/build-installers.yml   # the pipeline
packaging/windows/rebellion2.nsi         # NSIS installer script
packaging/linux/build-appimage.sh        # AppDir assembly + appimagetool
packaging/linux/rebellion2.desktop       # AppImage desktop entry
packaging/macos/build-zip.sh             # .app -> zip
```

Drop a real `packaging/linux/rebellion2.png` (256×256) and the AppImage will use it;
otherwise a solid-color placeholder icon is generated at build time.
