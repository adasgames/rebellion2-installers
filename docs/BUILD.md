# Build & release pipeline

How the installers in this repo are produced. For players, see the [README](../README.md).

Separate GitHub Actions workflows build **asset-free** game players from
[`rebellion2`](https://github.com/davidadas/rebellion2) and package the launcher in this
repository. OS-specific tags select which platform to build. Windows and macOS publish separate
application-update channels, so publishing one platform cannot strand the other. Both launchers
resolve immutable media for their own application version rather than blindly following another
platform's latest release. The expensive macOS runner remains opt-in. The packages ship **no art**
— the launcher downloads content from the configured distribution service after ownership
verification.

The **Windows** and **macOS** legs are live. Linux remains parked.

| Platform | Package                                | Tool                     | Status |
| -------- | -------------------------------------- | ------------------------ | ------ |
| Windows  | `Rebellion2-Windows-Setup.exe`         | Inno Setup (`ISCC.exe`)  | live   |
| macOS    | `Rebellion2-macOS.zip`                 | universal `.app` archive | live   |
| Linux    | `Rebellion2-<version>-x86_64.AppImage` | `appimagetool`           | parked |

The packages do not have trusted platform signatures yet, which is why Windows and macOS warn on
first launch. Windows application manifests and macOS updater archives do have independent
cryptographic signatures that the launchers verify before installing automatic updates.
Authenticode on Windows and Apple Developer ID signing/notarization on macOS remain on the roadmap.

## Triggering a build

The workflows have three entry points, and the difference matters:

- **Windows tag — cuts a Release.** Push a tag matching `windows-*` to _this_ repo:

  ```bash
  git tag windows-0.1.0
  git push origin windows-0.1.0
  ```

  This runs the Windows pipeline and publishes GitHub Release `v0.1.0`. Tag builds check out
  `v0.1.0` from both the game source and private build-asset source, so both repositories must
  carry the ordinary matching version tag before the installer tag is pushed.

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

  The workflow verifies that the release and its immutable application/content artifacts exist
  before starting either expensive build. This permits a missed macOS build to be backfilled after
  the live channel has advanced. It then attaches `Rebellion2-macOS.zip` to that release without
  changing the live release pointer. It also updates the prerelease `latest-macos` alias used by
  the README's stable macOS download link and the launcher's signed macOS update pointer.

- **Manual macOS dispatch — historical release/backfill.** Actions tab →
  **Build macOS Installer** → **Run workflow**, then enter an existing version such as `0.0.14`.
  The workflow verifies the immutable application and content artifacts for that version before it
  builds anything. Manual runs upload workflow artifacts but do not alter a Release unless the
  **publish** input is explicitly enabled. This is the supported way to publish the first
  auto-updating Mac build without recreating an old source tag.

## Jobs

1. **prepare** — resolves the version: the tag name minus its `windows-` prefix, else the
   `version` input, else `0.0.0-dev`. The workflow passes this value to the Unity player,
   launcher, content package, and installer so releases do not require a source-controlled
   version bump.
2. **player-windows** (`ubuntu-latest`) — calls the shared player workflow, checks out the game
   and private build assets, fetches their LFS objects, and installs them into `Assets/Content` plus
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
   content upload, stages the installer in a draft GitHub Release, and uploads the signed application
   layer. It converts second-level headings and their bullet lists from the draft Release description
   into a versioned release-notes JSON document when any are present. It embeds the matching content
   and optional release-notes pointers inside `application.json`, publishes that single release
   pointer, and verifies both versions through the direct and public channel. Only then does it
   publish the GitHub Release. Any failure restores the previous pointer; the installer remains
   hidden in its draft. Legacy `latest.json` remains pinned for older launchers.

## Release notes

Write launcher patch notes in the draft GitHub Release description. Use `##` headings for sections
and `*` or `-` bullets for individual changes. Other prose remains on GitHub but is not shown by the
launcher. For example:

```markdown
## Highlights

* Added new strategic options.

## Fixes

* Fixed interrupted manufacturing orders.
```

The release workflow converts that Markdown to `dist/release-notes-<version>.json`, records its path
and SHA-256 digest in the content portion of `dist/application.json`, and publishes both atomically.
An absent usable section omits the pointer. The launcher also ignores missing, corrupt, mismatched,
or malformed notes so release notes can never prevent an update.

The generated release-notes document has this schema:

```json
{
  "version": "x.x.xx",
  "sections": [
    {
      "title": "Highlights",
      "items": [
        "Added new strategic options."
      ]
    },
    {
      "title": "Fixes",
      "items": [
        "Fixed interrupted manufacturing orders."
      ]
    }
  ]
}
```

`version` must match the content release. `sections` must contain at least one object, and every
section must have a non-empty `title` and at least one string in `items`. The matching content object
in `application.json` references the document without embedding its display text:

```json
{
  "releaseNotes": {
    "path": "dist/release-notes-x.x.xx.json",
    "sha256": "<SHA-256 of the release-notes document>"
  }
}
```

The opt-in macOS workflow has its own cheap preflight and Ubuntu Unity-player job. Only its final
packaging job uses `macos-latest`; it embeds the Unity player before Tauri signs and packages the
complete universal application, verifies both direct-download and updater archives, attaches them
to the existing versioned release, and updates the stable `latest-macos` download alias. The alias
contains `latest-macos.json`, while its signed updater URL points at the immutable versioned
release. Existing Mac users may decline an application update and continue using media matching
their installed application version.

## Private release configuration

Release jobs depend on private source locations, service endpoints, storage credentials, signing
material, and build credentials configured in GitHub Actions. Their values and operational setup
are intentionally maintained outside this public repository. `GITHUB_TOKEN` is provided
automatically and publishes the Release. macOS updater builds require the
`TAURI_SIGNING_PRIVATE_KEY` repository secret; its matching public key is committed in the Tauri
configuration so installed launchers can reject forged updater archives.

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
packaging/macos/build-zip.sh               # complete signed macOS app -> direct-download archive
```
