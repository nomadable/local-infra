#!/bin/sh
# Hermetic smoke test for the release installer: no network and no user files.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/linf-installer-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT HUP INT TERM

FIXTURE="$TMP/fixture"
FAKE_BIN="$TMP/bin"
INSTALL_DIR="$TMP/install"
ASSET='linf-aarch64-apple-darwin.tar.gz'
mkdir -p "$FIXTURE/linf-vtest-aarch64-apple-darwin" "$FAKE_BIN"
printf '#!/bin/sh\nprintf "linf test\n"\n' > "$FIXTURE/linf-vtest-aarch64-apple-darwin/linf"
chmod 0755 "$FIXTURE/linf-vtest-aarch64-apple-darwin/linf"
(
  cd "$FIXTURE"
  tar -czf "$ASSET" linf-vtest-aarch64-apple-darwin
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$ASSET" > "$ASSET.sha256"
  else
    sha256sum "$ASSET" > "$ASSET.sha256"
  fi
)

cat > "$FAKE_BIN/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf 'Darwin\n' ;;
  -m) printf 'arm64\n' ;;
  *) exit 1 ;;
esac
EOF
chmod 0755 "$FAKE_BIN/uname"

cat > "$FAKE_BIN/curl" <<'EOF'
#!/bin/sh
set -eu
out=''
url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output)
      out="$2"
      shift 2
      ;;
    --proto)
      shift 2
      ;;
    --fail|--location|--silent|--show-error|--tlsv1.2)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
case "$url" in
  *linf-aarch64-apple-darwin.tar.gz.sha256)
    cp "$LINF_INSTALL_FIXTURE/linf-aarch64-apple-darwin.tar.gz.sha256" "$out"
    ;;
  *linf-aarch64-apple-darwin.tar.gz)
    cp "$LINF_INSTALL_FIXTURE/linf-aarch64-apple-darwin.tar.gz" "$out"
    ;;
  *)
    printf 'unexpected installer URL: %s\n' "$url" >&2
    exit 1
    ;;
esac
EOF
chmod 0755 "$FAKE_BIN/curl"

ENV_INSTALL_DIR="$TMP/env-install"
OUTPUT="$(
  PATH="$FAKE_BIN:$PATH" \
  LINF_INSTALL_FIXTURE="$FIXTURE" \
  LINF_INSTALL_DIR="$ENV_INSTALL_DIR" \
  sh "$ROOT/scripts/release-install.sh" --install-dir "$INSTALL_DIR"
)"

[ -x "$INSTALL_DIR/linf" ]
[ ! -e "$ENV_INSTALL_DIR/linf" ]
case "$OUTPUT" in
  *'설치 확인: linf --version'*'Docker 확인: linf doctor'*'터미널 앱 열기: linf'*'agent에서 사용: linf skill install'*)
    ;;
  *)
    printf 'missing post-install guidance:\n%s\n' "$OUTPUT" >&2
    exit 1
    ;;
esac
case "$OUTPUT" in
  *'export PATH="'*':$PATH"'*)
    ;;
  *)
    printf 'missing PATH guidance:\n%s\n' "$OUTPUT" >&2
    exit 1
    ;;
esac
