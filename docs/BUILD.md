# Build and release pipeline

How the installers and update channels in this repository are produced. For players, see the
[README](../README.md).

Launcher releases and game releases are independent:

- A **launcher release** has its own version, patch notes, signed Windows and macOS launcher
  layers, and `dist/launcher.json` pointer. It never builds or publishes the Unity player or game
  content.
- A **game release** has its own version, patch notes, signed Windows and macOS player layers,
  matching protected content, installers for both platforms, and `dist/game.json` pointer. It uses
  the latest published launcher when assembling fresh installers, but it never changes the launcher
  channel.

The packages ship no licensed art. The launcher verifies ownership before downloading protected
content. Windows and macOS are released together within each channel. Linux remains parked.

| Platform | Package                                | Tool                     | Status |
| -------- | -------------------------------------- | ------------------------ | ------ |
| Windows  | `Rebellion2-Windows-Setup.exe`         | Inno Setup (`ISCC.exe`)  | live   |
| macOS    | `Rebellion2-macOS.zip`                 | universal `.app` archive | live   |
| Linux    | `Rebellion2-<version>-x86_64.AppImage` | `appimagetool`           | parked |

The packages do not have trusted platform signatures yet, so Windows and macOS warn on first
launch. Every downloaded launcher and game file is nevertheless authenticated by a signed manifest
and a SHA-256 digest. Authenticode and Apple Developer ID signing/notarization remain on the roadmap.

## Building a launcher release

Actions tab → **Build launcher** → **Run workflow**.

- **version** is the independent launcher version, such as `1.0.1`.
- **release_notes** contains launcher-only Markdown notes.
- **publish** controls whether the run moves the live launcher channel.
- **allow_rollback** is reserved for an intentional emergency rollback.

The workflow builds the Windows and universal macOS launchers together. A non-publishing run uploads
workflow artifacts only. A publishing run creates or updates GitHub Release `launcher-v<version>`,
uploads signed manifests and content-addressed blobs, publishes launcher-only release notes, and
moves `dist/launcher.json` last.

Installed launchers display this as a **Launcher update**. They download and stage it without
restarting or closing. The same session then checks for a game update and always leaves the player
with a **Play game** action. The staged launcher applies the next time the launcher starts.

## Building a game release

Actions tab → **Build game** → **Run workflow**.

- **source_ref** selects the `rebellion2` revision.
- **asset_ref** selects the matching `rebellion2-media` revision.
- **version** is the game/content version, such as `0.0.26`.
- **release_notes** contains game-only Markdown notes.
- **publish** controls whether the run moves the live game channel.
- **allow_rollback** is reserved for an intentional emergency rollback.

The workflow builds Windows and macOS players in parallel, packages a signed player layer for each,
and assembles both fresh installers with the current published launcher. When publishing, it also
uploads the immutable protected content archive, manifest, and blobs. It creates a draft GitHub
Release, uploads both installers, publishes `dist/game.json` only after every immutable artifact is
available, verifies the public pointer, and then publishes the GitHub Release.

`dist/game.json` binds one game version to all of the following:

- The signed Windows player manifest and public game blobs.
- The signed macOS player manifest and public game blobs.
- The matching protected content manifest and blobs.
- The game release-notes document.

Publishing a game release cannot move `dist/launcher.json`. Publishing a launcher release cannot
move `dist/game.json`, publish content, or replace a game player.

## Release notes

Both workflows accept Markdown with `##` section headings and `*` or `-` bullets. Other prose may
appear on GitHub but is not shown in the launcher. For example:

```markdown
## Additions

* Added new strategic options.

## Fixes

* Fixed interrupted manufacturing orders.
```

Game notes are published as `dist/release-notes-<version>.json`. Launcher notes are published as
`dist/release-notes-launcher-<version>.json`. Their histories are collected from separate GitHub
tag families (`v<version>` and `launcher-v<version>`), so launcher changes never appear in game
notes and game changes never appear in launcher notes.

The generated document has this schema:

```json
{
  "version": "1.0.1",
  "sections": [
    {
      "title": "Fixes",
      "items": ["Kept the launcher open after downloading an update."]
    }
  ],
  "releases": [
    {
      "version": "1.0.1",
      "sections": [
        {
          "title": "Fixes",
          "items": ["Kept the launcher open after downloading an update."]
        }
      ]
    }
  ]
}
```

The pointer records the document path and digest. The launcher ignores missing, corrupt,
mismatched, or malformed notes so notes cannot prevent an update. When an installation skips game
or launcher versions, the corresponding view combines only that channel's newer notes.

## Local launcher walkthrough

Run the debug launcher with `--preview-update-flow` to inspect the split flow without contacting the
live channel or writing update files:

```bash
cargo run --manifest-path launcher/src-tauri/Cargo.toml -- --preview-update-flow
```

The walkthrough shows a launcher update and launcher notes, simulates staging it, proceeds to a game
update with separate game notes, and finishes on the ready-to-play screen.

## Legacy transition

`build-windows-installer.yml` and `build-macos-installer.yml` remain temporarily available only to
deliver the first split-aware launcher to installations that know the former combined application
channel. After that bridge release is deployed, all normal releases use `build-launcher.yml` or
`build-game.yml`. The legacy channel stays pinned so it cannot couple later launcher and game
releases.

## Private release configuration

Release jobs use repository secrets for source repositories, ownership and content endpoints,
storage, signing keys, and Unity credentials. Their values and operational setup remain outside this
public repository. `GITHUB_TOKEN` is supplied automatically for GitHub Releases.

## Layout

```text
.github/workflows/build-launcher.yml          # independent cross-platform launcher release
.github/workflows/build-game.yml              # cross-platform game, content, and installers
.github/workflows/build-player.yml            # shared Unity player build
.github/workflows/build-windows-installer.yml # temporary legacy transition
.github/workflows/build-macos-installer.yml   # temporary legacy transition
launcher/                                     # Tauri launcher source
launcher/self-update/                         # staged-launcher handoff helper
launcher/update-core/                         # signed manifest diff/apply engine
packaging/windows/rebellion2-launcher.iss     # Windows installer
packaging/macos/build-zip.sh                  # complete macOS app archive
```
