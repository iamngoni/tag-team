# Releasing Tag Team

Tag Team publishes native archives from GitHub Actions. A release is a Git tag named `vX.Y.Z`; the workflow runs the same checks as CI, packages both supported targets, verifies the archives, and creates the GitHub release.

## Supported release targets

| Archive target | GitHub runner | Minimum runtime |
|---|---|---|
| `aarch64-apple-darwin` | `macos-15` | macOS 14 on Apple Silicon |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Linux x86_64 with glibc 2.35+; `bubblewrap` and per-user systemd on nodes |

The runner includes the Claude Agent SDK's platform-native production dependencies. Builds must therefore run on the target OS and architecture; the packaging script rejects cross-target builds rather than producing an archive that cannot run.

Node.js 22+ is a runtime prerequisite and is not bundled. The Rust binaries are included; they rely on the target platform's system libraries.

## Archive contents

Each archive has one top-level directory named after the archive, for example:

```text
tagteam-v0.1.0-aarch64-apple-darwin/
  README.md
  install-release.sh
  bin/tagteam
  bin/tagteam-node
  runner/dist/src/main.js
  runner/node_modules/
  plugin/
  docs/
  apps/tagteam-menubar/README.md
  TagTeam.app/             # macOS archive only
  SHA256SUMS
```

The matching `.sha256` sidecar is published next to the archive. The payload's `SHA256SUMS` manifest verifies every file before installation.

## Local packaging

From the repository root on a supported host:

```bash
./scripts/package-release.sh --version 0.1.0
```

Outputs:

```text
dist/tagteam-v0.1.0-aarch64-apple-darwin.tar.gz
dist/tagteam-v0.1.0-aarch64-apple-darwin.tar.gz.sha256
```

Use `--target` only to assert the expected native target. `--output-dir` changes the destination. The script builds release Rust binaries, builds the runner from a clean staging copy, installs production-only runner dependencies, builds the macOS app when applicable, and signs the macOS binaries and app ad hoc.

## Verify an archive locally

```bash
shasum -a 256 -c dist/tagteam-v0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf dist/tagteam-v0.1.0-aarch64-apple-darwin.tar.gz
cd tagteam-v0.1.0-aarch64-apple-darwin
shasum -a 256 -c SHA256SUMS
TAGTEAM_INSTALL_PREFIX="$PWD/install-test" TAGTEAM_SKIP_APP=1 ./install-release.sh
install-test/bin/tagteam --version
install-test/bin/tagteam-node --config /dev/null --version
```

On Linux, use `sha256sum -c` instead of `shasum -a 256 -c`.

## CI

`.github/workflows/ci.yml` runs on pull requests, pushes to `main`, and manual dispatch. It calls the reusable checks workflow for:

- Rust formatting, clippy, and the workspace test suite on macOS arm64 and Linux x86_64;
- runner typecheck and tests on Linux;
- Swift tests for the menu bar app on macOS.

When checks pass, the reusable package workflow uploads native archives and their checksums as workflow artifacts. Pull-request artifacts expire with the workflow retention policy and are not GitHub Releases.

`.github/workflows/release.yml` runs for `v*` tags. Manual dispatch builds the archives without publishing. A tag runs the same checks and packaging before the publish job creates or updates the GitHub Release with the archives and sidecars.

## Publish a tagged release

1. Ensure the versions in `crates/tagteam-hub/Cargo.toml`, `runner/package.json`, and `plugin/.claude-plugin/plugin.json` describe the same product version. The release workflow fails before packaging if they differ.
2. Commit the release changes and push `main`.
3. Create and push a tag:

```bash
git tag -a v0.1.0 -m "Tag Team v0.1.0"
git push origin v0.1.0
```

4. Watch the `Release` workflow. The publish job verifies each archive sidecar before calling `gh release create` or `gh release upload`.

Do not create the GitHub Release manually before the workflow finishes; the workflow is the release builder of record.

## Release limitations

- The menu bar app is ad-hoc signed and not notarized. Users may need to approve it in macOS System Settings.
- Linux archives target glibc 2.35+; musl and Linux arm64 are not published yet.
- The plugin has no marketplace manifest, so installation remains session-scoped with `claude --plugin-dir`.
- Tag Team is currently `UNLICENSED`; no license is granted by a public repository or downloadable build.
- A successful local package build is not CI proof, and a successful CI build is not deployment or signed-in provider proof. Record those checks separately.
