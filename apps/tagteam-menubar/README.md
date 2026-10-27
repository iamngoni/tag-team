# Tag Team menu bar app

A native macOS 14+ menu bar switch for an enrolled lent machine. It uses SwiftUI, AppKit, and Foundation only. The status core and command runner have no external dependencies.

## Build and test

From this directory:

```sh
swift build
swift test
./Scripts/build-app.sh
```

The helper creates `dist/TagTeam.app`, writes its Info.plist, and applies an ad-hoc code signature. Move `dist/TagTeam.app` to `/Applications` to install, then launch it from Finder or with `open /Applications/TagTeam.app`.

## Node CLI

The app searches `~/.local/bin`, the inherited `PATH`, and common Homebrew and system binary directories for `tagteam-node`. It reads `tagteam-node status` every three seconds. Pause, resume, and leave map to the corresponding fixed CLI commands. Leave always asks for confirmation.

Service controls call `tagteam-node service install`, `start`, `stop`, `status --json`, and `uninstall`. Install and uninstall ask for confirmation. Optional capacity, workspace count, connection, and policy details are displayed only when returned by the CLI.

Open config uses `~/.config/tagteam/node.toml`. Copy node ID writes only to the macOS pasteboard. The app does not use a shell, web view, placeholder state, or sample node data.
