#!/bin/sh
# Published as linf-installer.sh alongside every GitHub Release.
set -eu

REPOSITORY="${LINF_REPOSITORY:-nomadable/local-infra}"
INSTALL_DIR="${LINF_INSTALL_DIR:-$HOME/.local/bin}"
VERSION=""

usage() {
  cat <<'EOF'
Usage: linf-installer.sh [--version vX.Y.Z] [--install-dir DIR]

Installs linf from the latest GitHub Release by default.
EOF
}

fail() {
  printf 'linf installer: %s\n' "$*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || fail '--version requires vX.Y.Z'
      VERSION="$2"
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || fail '--install-dir requires a directory'
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

[ "$(id -u)" -ne 0 ] || fail 'do not run the installer as root; choose a user install directory instead'

for command in curl tar install uname mktemp; do
  command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done
if command -v shasum >/dev/null 2>&1; then
  CHECKSUM=shasum
elif command -v sha256sum >/dev/null 2>&1; then
  CHECKSUM=sha256sum
else
  fail 'required command not found: shasum or sha256sum'
fi

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    TARGET='aarch64-apple-darwin'
    ;;
  Darwin:x86_64)
    TARGET='x86_64-apple-darwin'
    ;;
  Linux:x86_64)
    TARGET='x86_64-unknown-linux-gnu'
    ;;
  *)
    fail "unsupported platform: $(uname -s) $(uname -m)"
    ;;
esac

case "$VERSION" in
  '')
    RELEASE_URL="https://github.com/${REPOSITORY}/releases/latest/download"
    ;;
  v[0-9]*)
    RELEASE_URL="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
    ;;
  *)
    fail '--version must be an annotated tag such as v0.1.0'
    ;;
esac

ASSET="linf-${TARGET}.tar.gz"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/linf-install.XXXXXX")"
cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT HUP INT TERM

printf 'Downloading linf for %s…\n' "$TARGET"
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
  "${RELEASE_URL}/${ASSET}" \
  --output "${WORK_DIR}/${ASSET}"
curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
  "${RELEASE_URL}/${ASSET}.sha256" \
  --output "${WORK_DIR}/${ASSET}.sha256"

(
  cd "$WORK_DIR"
  if [ "$CHECKSUM" = shasum ]; then
    shasum -a 256 -c "${ASSET}.sha256"
  else
    sha256sum -c "${ASSET}.sha256"
  fi
)

tar -xzf "${WORK_DIR}/${ASSET}" -C "$WORK_DIR"
BINARY=''
for candidate in "$WORK_DIR"/*/linf; do
  if [ -x "$candidate" ]; then
    BINARY="$candidate"
    break
  fi
done
[ -n "$BINARY" ] || fail 'release archive did not contain an executable linf binary'

mkdir -p "$INSTALL_DIR"
install -m 0755 "$BINARY" "$INSTALL_DIR/linf"
printf 'Installed linf to %s/linf\n' "$INSTALL_DIR"
printf 'Run: %s/linf --version\n' "$INSTALL_DIR"
