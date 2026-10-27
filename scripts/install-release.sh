#!/usr/bin/env bash
# Installs an unpacked Tag Team release without overwriting user configuration.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${TAGTEAM_INSTALL_PREFIX:-$HOME/.local}"
if [[ "$PREFIX" != /* ]]; then
  PREFIX="$PWD/$PREFIX"
fi
BIN_DIR="$PREFIX/bin"
SHARE_DIR="$PREFIX/share/tagteam"
APP_SOURCE="$SCRIPT_DIR/TagTeam.app"
APP_DEST="${TAGTEAM_APP_DEST:-$HOME/Applications/TagTeam.app}"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

if [[ ! -f "$SCRIPT_DIR/SHA256SUMS" ]]; then
  die "SHA256SUMS is missing; unpack the complete release archive"
fi
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$SCRIPT_DIR" && sha256sum -c SHA256SUMS >/dev/null)
elif command -v shasum >/dev/null 2>&1; then
  (cd "$SCRIPT_DIR" && shasum -a 256 -c SHA256SUMS >/dev/null)
else
  die "sha256sum or shasum is required to verify the release"
fi

for required in bin/tagteam bin/tagteam-node runner/dist/src/main.js plugin/.mcp.json docs/README.md; do
  [[ -e "$SCRIPT_DIR/$required" ]] || die "release is incomplete: missing $required"
done

mkdir -p "$BIN_DIR" "$SHARE_DIR"
install -m 0755 "$SCRIPT_DIR/bin/tagteam" "$BIN_DIR/tagteam"
install -m 0755 "$SCRIPT_DIR/bin/tagteam-node" "$BIN_DIR/tagteam-node"
rm -rf "$SHARE_DIR/runner" "$SHARE_DIR/plugin" "$SHARE_DIR/docs" "$SHARE_DIR/apps"
cp -a "$SCRIPT_DIR/runner" "$SHARE_DIR/runner"
cp -a "$SCRIPT_DIR/plugin" "$SHARE_DIR/plugin"
cp -a "$SCRIPT_DIR/docs" "$SHARE_DIR/docs"
cp -a "$SCRIPT_DIR/apps" "$SHARE_DIR/apps"
cp "$SCRIPT_DIR/README.md" "$SHARE_DIR/README.md"

installed_app=""
if [[ "$(uname -s)" == "Darwin" && -d "$APP_SOURCE" && "${TAGTEAM_SKIP_APP:-0}" != "1" ]]; then
  mkdir -p "$(dirname "$APP_DEST")"
  rm -rf "$APP_DEST"
  cp -a "$APP_SOURCE" "$APP_DEST"
  installed_app="$APP_DEST"
fi

"$BIN_DIR/tagteam" --version >/dev/null
"$BIN_DIR/tagteam-node" --config /dev/null --version >/dev/null

NODE_PATH="$(command -v node || true)"
RUNNER_PATH="$SHARE_DIR/runner/dist/src/main.js"
PLUGIN_PATH="$SHARE_DIR/plugin"

printf '\nInstalled Tag Team into %s\n' "$PREFIX"
printf '  binaries: %s\n' "$BIN_DIR"
printf '  runner:   %s\n' "$RUNNER_PATH"
printf '  plugin:   %s\n' "$PLUGIN_PATH"
if [[ -n "$installed_app" ]]; then
  printf '  app:      %s (ad-hoc signed, not notarized)\n' "$installed_app"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '%s\n' "" "Add this directory to PATH before starting Claude Code:" "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

printf '\nNext steps:\n'
printf '  1. Create ~/.config/tagteam/hub.toml with an absolute runner path.\n'
if [[ -n "$NODE_PATH" ]]; then
  printf '     runner_program = "%s"\n' "$NODE_PATH"
else
  printf '     runner_program = "/absolute/path/to/node"\n'
fi
printf '     runner_args = ["%s"]\n' "$RUNNER_PATH"
printf '  2. Start the hub in a terminal with tagteam hub, or install its user service:\n'
printf '     tagteam service install && tagteam service start\n'
printf '  3. On a node, configure ~/.config/tagteam/node.toml, then run tagteam-node run\n'
printf '     or install its user service: tagteam-node service install && tagteam-node service start\n'
printf '  4. Load the plugin in Claude Code with:\n'
printf '     claude --plugin-dir "%s"\n' "$PLUGIN_PATH"
printf '  Full setup instructions: %s/README.md\n' "$SHARE_DIR"
