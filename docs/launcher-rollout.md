# Launcher hotfix rollout

The candidate is launcher 1.0.7. The latest published launcher checked during preparation was
1.0.6, and the game release was 0.0.38. Recheck the live channel before publishing.

1. Finish review and validation of the exact launcher revision to release.
2. Run **Build launcher** with version `1.0.7`, the Markdown from `launcher-hotfix.md`,
   `publish=false`, and `allow_rollback=false`. Require all platform checks to pass.
3. Publish that reviewed revision through **Build launcher**. Its workflow publishes immutable
   launcher files and signed manifests before moving `dist/launcher.json`.
4. Confirm an installed 0.0.26 launcher downloads the new launcher and helper. Close and reopen
   it before approving the game update: that installed launcher applies its replacement on restart.
5. Check both an existing Content installation and an older game with no Content. Require the
   latter to download one matching archive, and reject outdated archives without writing a new
   Content version. Check that damaged current-version Content offers repair.
6. Run **Refresh Windows installer** with `game_version=0.0.38`, `launcher_version=1.0.7`, and
   `publish=false`. It restores and verifies the already released player, verifies the published
   launcher bundle, and compiles a replacement installer. Test that artifact over an existing
   installation and as a fresh installation.
7. After review, run the same workflow with `publish=true`. It replaces only
   `Rebellion2-Windows-Setup.exe` on the existing game release. It does not rebuild the game,
   republish Content, move update channels, or change game release notes.

The notes generator produces one JSON document for both launcher generations: older launchers
read `sections`, and newer launchers also read `releases`. Generate a preview with:

```powershell
node scripts/release-notes.mjs docs/launcher-hotfix.md release-notes-launcher-1.0.7.json 1.0.7
```

Local Windows runtime tests compile the launcher and helper at tag `v0.0.26`, changing only their
test signing key and service configuration. They exercise the real replacement and restart path
with and without Content. They also compile and run the production Inno template with isolated
fixture payloads. These fixtures are test installers, not public release packages. Native macOS
and Linux release validation remains required before publishing the cross-platform launcher.
