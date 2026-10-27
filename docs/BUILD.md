# Build and release pipeline

How the installers and update channels in this repository are produced. For players, see the
[README](../README.md).

Launcher releases and game releases are independent:

- A **launcher release** has its own version, patch notes, signed Windows, macOS, and Linux launcher
  layers, and `dist/launcher.json` pointer. It never builds or publishes the Unity player or game
  content.
- A **game release** has its own version, patch notes, signed Windows, macOS, and Linux player
  layers, matching protected content, packages for all three platforms, and `dist/game.json`
  pointer. It uses
  the latest published launcher when assembling fresh installers, but it never changes the launcher
  channel.

The packages ship no licensed art. The launcher verifies ownership before downloading protected
content. Windows, macOS, and Linux are released together within each channel.

| Platform | Package                                | Tool                     | Status |
| -------- | -------------------------------------- | ------------------------ | ------ |
| Windows  | `Rebellion2-Windows-Setup.exe`         | Inno Setup (`ISCC.exe`)  | live   |
| macOS    | `Rebellion2-macOS.zip`                 | universal `.app` archive | live   |
| Linux    | `Rebellion2-<version>-x86_64.AppImage` | `appimagetool`           | live   |

The packages do not have trusted platform signatures yet, so operating systems may warn on first
launch. Every downloaded launcher and game file is nevertheless authenticated by a signed manifest
and a SHA-256 digest. Authenticode and Apple Developer ID signing/notarization remain on the roadmap.

## Building a launcher release

Actions tab → **Build launcher** → **Run workflow**.

- **version** is the independent launcher version, such as `1.0.1`.
- **release_notes** contains launcher-only Markdown notes.
- **publish** controls whether the run moves the live launcher channel.
- **allow_rollback** is reserved for an intentional emergency rollback.

The workflow builds the Windows, universal macOS, and Linux launchers together. A non-publishing
run uploads workflow artifacts only. A publishing run creates or updates GitHub Release
`launcher-v<version>`, uploads signed manifests and content-addressed blobs, publishes launcher-only
release notes, and moves `dist/launcher.json` last.

Installed launchers display this as a **Launcher update**. After downloading and verifying the
replacement, the launcher restarts before checking for game updates. It restores its window position,
cached sign-in, pending first-install archive, and update choices, then continues the update flow.
If the helper cannot start, a retry screen keeps the older launcher from continuing into game updates.

## Building a game release

Actions tab → **Build game** → **Run workflow**.

- **source_ref** selects the `rebellion2` revision.
- **asset_ref** selects the matching `rebellion2-media` revision.
- **version** is the game/content version, such as `0.0.26`.
- **release_notes** contains game-only Markdown notes.
- **publish** controls whether the run moves the live game channel.
- **allow_rollback** is reserved for an intentional emergency rollback.

The workflow builds Windows, macOS, and Linux players in parallel, packages a signed player layer
for each, and assembles fresh packages with the current published launcher. The Linux AppImage
bootstraps the player into the user's XDG data directory because mounted AppImages are read-only;
later launcher and game updates replace their independently signed layers. When publishing, the
workflow also uploads the immutable protected content archive, manifest, and blobs. It creates a
draft GitHub Release, uploads all three packages, publishes `dist/game.json` only after every
immutable artifact is available, verifies the public pointer, and then publishes the GitHub Release.

`dist/game.json` binds one game version to all of the following:

- The signed Windows player manifest and public game blobs.
- The signed macOS player manifest and public game blobs.
- The signed Linux player manifest and public game blobs.
- The matching protected content manifest and blobs.
- The game release-notes document.

Publishing a game release cannot move `dist/launcher.json`. Publishing a launcher release cannot
move `dist/game.json`, publish content, or replace a game player.

## Backfilling Linux for an existing game release

Actions tab → **Backfill Linux game** → **Run workflow**.

Use this workflow only when an existing game version needs its first Linux package. Select the
matching `rebellion2` and `rebellion2-media` release refs, the existing game version, and the
published launcher version to embed. The workflow builds only the Linux player and AppImage. A
publishing run requires explicit backfill confirmation, refuses to replace an existing Linux game
channel entry, and uploads the AppImage. If `dist/game.json` already exists for that game version,
the run also uploads the signed Linux player layer and adds Linux while preserving every existing
platform entry. If the split game channel does not exist, the run leaves all live channel pointers
unchanged so older clients continue using the legacy application channel.

## Backfilling macOS for an existing game release

Actions tab → **Backfill macOS game** → **Run workflow**.

Supply the matching game and media refs, existing game version, and published launcher version.
The workflow uses the standard macOS player and launcher packaging, verifies bundle signing, and
adds only the missing macOS package and signed update layer. Publication requires backfill
confirmation and preserves the existing Windows/Linux entries, matching content, and release notes.
Backfill workflows share the game-release concurrency group so their channel updates cannot race.

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

## Local native update tests

On Windows with Node 24+, Rust, the Tauri build prerequisites, WebView2, and Inno Setup 6 installed, run:

```powershell
node scripts/test-launcher-runtime.mjs
```

This builds and drives the actual debug launcher and self-update helper against a loopback server.
It uses isolated temporary installations, a stub game, and fixture signing keys. It does not publish
updates or touch an installed game. Release builds always use the production verification key.

The scenarios cover fresh installation, repair, content packs, independent and combined updates,
helper relaunch, interrupted downloads and commits, simultaneous launchers, locked files,
authorization renewal, invalid signatures and paths, corrupt data, HTTP resume, and failed servers.
They also compile the production installer template with isolated fixture payloads and exercise
fresh installs, reinstalls, interrupted-update recovery, in-use files, and installation locking.
They compile tag `v0.0.26` with fixture signing and service settings to test upgrades from that
launcher with and without Content. Matching archives, damaged current-version installations, and
full-inventory verification are covered too. The migration sequence is in
[launcher-rollout.md](launcher-rollout.md).

Every Content install, update, and explicit repair verifies the complete resulting inventory
before its transaction is completed. A verification record is bound to the exact saved manifest.
Normal launches reuse that record instead of reading all Content again; installations without
one, or whose saved manifest has changed, are checked against the published inventory first.
Set `ISCC_PATH` if the Inno Setup compiler is not in its per-user default directory.
For the destructive disk-full fixture, use only a disposable virtual volume smaller than 1 GB.
Create `.reb2-runtime-volume` at its root containing exactly `reb2-runtime-only`, then set
`REB2_RUNTIME_DISK_ROOT` to its drive root and select the `full disk` scenario. It fills that
volume, checks that live files survive, frees the filler, and retries the saved download.
An optional text argument selects matching scenario names. Run the suite serially because each run
builds a launcher with that run's loopback address.

Each run prints its temporary artifact directory. `results.json` records outcomes; `build.json`
records source and binary hashes, compiler versions, and the fixture address. `ready.png` captures
the rendered ready screen. Failed runs retain launcher logs and UI diagnostics. These native tests
exercise Windows only; macOS and Linux require their own native validation.

For a software-rendered test run, set `REB2_RUNTIME_BROWSER_ARGUMENTS=--disable-gpu` in the test
process's environment. The report records that override; it does not change launcher release builds.

## Steam verification recovery

Steam visibility failures use the ownership service's `reason` query parameter on `/done`:
`steam_library_unavailable` opens privacy recovery, and `steam_game_not_found` explains that
neither eligible game appeared in the shared list. Other failures retain the service's message.
The service also checks profile visibility after these failures and returns it separately as
`profile_visibility=public`, `not_public`, or `unknown`. The screen displays profile visibility and
game-list visibility independently. A failed profile lookup is shown as unavailable, and an
unreadable game list does not expose its exact Private or Friends Only setting.

The recovery screen returns to bundled launcher content before displaying its actions.
**Open Steam privacy settings** opens `https://steamcommunity.com/my/edit/settings` in the
system browser. If opening fails, the screen explains how to reach privacy settings in Steam.
The player must sign in to the owning account in that browser. **Retry** starts a fresh Steam
sign-in while retaining the requested content version and pending download or update.

The companion ownership-service change must be deployed for these reason-specific screens to
appear. Existing service responses continue using the generic failure screen. No launcher
release or ownership-service deployment is performed by opening a pull request.

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
packaging/linux/build-appimage.sh             # complete Linux AppImage
packaging/linux/AppRun                        # writable Linux installation bootstrap
```
