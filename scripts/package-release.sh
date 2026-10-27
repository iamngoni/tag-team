#!/usr/bin/env bash
# Builds a self-contained, platform-native Tag Team release archive.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$ROOT_DIR/dist"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/crates/tagteam-hub/Cargo.toml" | head -n 1)"
TARGET=""

usage() {
  cat <<'EOF'
Usage: scripts/package-release.sh [options]

Options:
  --version <x.y.z>    Version written into the archive name and macOS app.
  --target <triple>    Release target (default: this machine's Rust host).
  --output-dir <dir>   Directory for the archive and checksum (default: ./dist).
  -h, --help           Show this help.

Supported native targets:
  aarch64-apple-darwin
  x86_64-unknown-linux-gnu

The runner ships platform-native production dependencies, so release builds must
run on the target OS and architecture. Cross-packaging is intentionally refused.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || die "--version requires a value"
      VERSION="$2"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    --target)
      [[ $# -ge 2 ]] || die "--target requires a value"
      TARGET="$2"
      shift 2
      ;;
    --target=*)
      TARGET="${1#*=}"
      shift
      ;;
    --output-dir)
      [[ $# -ge 2 ]] || die "--output-dir requires a value"
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --output-dir=*)
      OUTPUT_DIR="${1#*=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

VERSION="${VERSION#v}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][0-9A-Za-z.-]+)?$ ]] || die "invalid semantic version: $VERSION"

HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"
TARGET="${TARGET:-$HOST_TARGET}"
case "$TARGET" in
  aarch64-apple-darwin|x86_64-unknown-linux-gnu) ;;
  *) die "unsupported target: $TARGET" ;;
esac
[[ "$TARGET" == "$HOST_TARGET" ]] || die "target $TARGET does not match build host $HOST_TARGET; build each release on its native runner"

for tool in cargo pnpm node tar; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done
if [[ -n "${PROTOC:-}" ]]; then
  [[ -x "$PROTOC" ]] || die "PROTOC does not point to an executable: $PROTOC"
else
  command -v protoc >/dev/null 2>&1 || die "protoc is required to build the Rust binaries"
fi
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  die "sha256sum or shasum is required"
fi
if [[ "$TARGET" == *-apple-darwin ]]; then
  command -v codesign >/dev/null 2>&1 || die "codesign is required for a macOS release"
fi

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT_DIR/target}"
if [[ "$CARGO_TARGET_DIR" != /* ]]; then
  CARGO_TARGET_DIR="$PWD/$CARGO_TARGET_DIR"
fi
OUTPUT_DIR="$(mkdir -p "$OUTPUT_DIR" && cd "$OUTPUT_DIR" && pwd)"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/tagteam-release.XXXXXX")"
cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

PACKAGE_NAME="tagteam-v${VERSION}-${TARGET}"
PAYLOAD_DIR="$WORK_DIR/$PACKAGE_NAME"
RUNNER_BUILD_DIR="$WORK_DIR/runner-build"
mkdir -p "$PAYLOAD_DIR"/{bin,runner,plugin,docs}

printf 'Building Tag Team %s for %s\n' "$VERSION" "$TARGET"

# Build the two user-facing Rust binaries in release mode from the locked graph.
cargo build --release --locked -p tagteam-hub -p tagteam-node
BINARY_DIR="$CARGO_TARGET_DIR/release"
cp "$BINARY_DIR/tagteam" "$PAYLOAD_DIR/bin/tagteam"
cp "$BINARY_DIR/tagteam-node" "$PAYLOAD_DIR/bin/tagteam-node"
chmod 0755 "$PAYLOAD_DIR/bin/tagteam" "$PAYLOAD_DIR/bin/tagteam-node"

# Build the TypeScript runner from a clean staging copy so the checkout's
# node_modules directory is never mutated by a release.
mkdir -p "$RUNNER_BUILD_DIR"
cp "$ROOT_DIR/runner/package.json" "$ROOT_DIR/runner/pnpm-lock.yaml" \
  "$ROOT_DIR/runner/tsconfig.json" "$RUNNER_BUILD_DIR/"
cp -R "$ROOT_DIR/runner/src" "$RUNNER_BUILD_DIR/src"
pnpm -C "$RUNNER_BUILD_DIR" install --frozen-lockfile --ignore-scripts
pnpm -C "$RUNNER_BUILD_DIR" run build

mkdir -p "$PAYLOAD_DIR/runner/dist"
cp "$RUNNER_BUILD_DIR/package.json" "$RUNNER_BUILD_DIR/pnpm-lock.yaml" "$PAYLOAD_DIR/runner/"
cp -R "$RUNNER_BUILD_DIR/dist/src" "$PAYLOAD_DIR/runner/dist/src"
# Install only runtime dependencies into the payload. The SDK's native package
# is selected by pnpm on this target machine and must travel with the archive.
pnpm -C "$PAYLOAD_DIR/runner" install --prod --frozen-lockfile --ignore-scripts

# Fail the package build if the staged runner cannot even load its runtime graph.
node --input-type=module - "$PAYLOAD_DIR/runner/dist/src/main.js" <<'NODE'
import { pathToFileURL } from "node:url";
await import(pathToFileURL(process.argv[2]).href);
NODE

cp "$ROOT_DIR/README.md" "$PAYLOAD_DIR/README.md"
cp -R "$ROOT_DIR/plugin/." "$PAYLOAD_DIR/plugin/"
cp -R "$ROOT_DIR/docs/." "$PAYLOAD_DIR/docs/"
# Preserve the documentation index's menu-bar link in the installed docs tree.
mkdir -p "$PAYLOAD_DIR/apps/tagteam-menubar"
cp "$ROOT_DIR/apps/tagteam-menubar/README.md" "$PAYLOAD_DIR/apps/tagteam-menubar/README.md"
cp "$ROOT_DIR/scripts/install-release.sh" "$PAYLOAD_DIR/install-release.sh"
chmod 0755 "$PAYLOAD_DIR/install-release.sh"

if [[ "$TARGET" == *-apple-darwin ]]; then
  # CFBundleShortVersionString is a numeric version, not a semantic version
  # with a prerelease suffix. CI package versions use the numeric base.
  APP_VERSION="${VERSION%%[-+]*}"
  TAGTEAM_BUILD_DIR="$WORK_DIR/swift-build" \
    TAGTEAM_APP_BUNDLE="$PAYLOAD_DIR/TagTeam.app" \
    TAGTEAM_APP_VERSION="$APP_VERSION" \
    TAGTEAM_APP_BUILD_VERSION="${TAGTEAM_APP_BUILD_VERSION:-1}" \
    "$ROOT_DIR/apps/tagteam-menubar/Scripts/build-app.sh"
  # Ad-hoc signatures make the downloaded CLI binaries launchable on macOS
  # without claiming notarization or a Developer ID identity.
  codesign --force --sign - "$PAYLOAD_DIR/bin/tagteam" "$PAYLOAD_DIR/bin/tagteam-node"
fi

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

printf 'Writing payload checksums\n'
(
  cd "$PAYLOAD_DIR"
  find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort | while IFS= read -r file; do
    printf '%s  %s\n' "$(sha256_file "$file")" "$file"
  done
) > "$PAYLOAD_DIR/SHA256SUMS"

ARCHIVE="$OUTPUT_DIR/$PACKAGE_NAME.tar.gz"
printf 'Creating %s\n' "$ARCHIVE"
COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$WORK_DIR" "$PACKAGE_NAME"
printf '%s  %s\n' "$(sha256_file "$ARCHIVE")" "$(basename "$ARCHIVE")" > "$ARCHIVE.sha256"

# Exercise the same flow a user will run: verify the sidecar, unpack, verify the
# payload manifest through the installer, and execute the installed binaries and
# runner entry point.
VERIFY_DIR="$WORK_DIR/verify"
VERIFY_EXTRACT="$VERIFY_DIR/extract"
VERIFY_PREFIX="$VERIFY_DIR/prefix"
VERIFY_APP="$VERIFY_DIR/Applications/TagTeam.app"
mkdir -p "$VERIFY_EXTRACT"
(
  cd "$OUTPUT_DIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$(basename "$ARCHIVE").sha256" >/dev/null
  else
    shasum -a 256 -c "$(basename "$ARCHIVE").sha256" >/dev/null
  fi
)
tar -xzf "$ARCHIVE" -C "$VERIFY_EXTRACT"
TAGTEAM_INSTALL_PREFIX="$VERIFY_PREFIX" TAGTEAM_APP_DEST="$VERIFY_APP" \
  "$VERIFY_EXTRACT/$PACKAGE_NAME/install-release.sh"
"$VERIFY_PREFIX/bin/tagteam" --version
"$VERIFY_PREFIX/bin/tagteam-node" --config /dev/null --version
node --input-type=module - "$VERIFY_PREFIX/share/tagteam/runner/dist/src/main.js" <<'NODE'
import { pathToFileURL } from "node:url";
await import(pathToFileURL(process.argv[2]).href);
console.log("runner import: ok");
NODE
if [[ "$TARGET" == *-apple-darwin ]]; then
  plutil -lint "$VERIFY_APP/Contents/Info.plist" >/dev/null
  codesign --verify --deep --strict "$VERIFY_APP"
fi

printf 'Created:\n  %s\n  %s\n' "$ARCHIVE" "$ARCHIVE.sha256"
