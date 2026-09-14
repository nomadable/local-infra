#!/bin/sh
# Hermetic smoke test for the release installer: no network and no user files.
# Every supported `uname -s`/`uname -m` pair must map to the archive the
# release workflow actually publishes.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/linf-installer-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT HUP INT TERM

# platform_case <uname -s> <uname -m> <expected target triple>
platform_case() {
  os="$1"
  machine="$2"
  target="$3"
  case_dir="$TMP/$os-$machine"
  fixture="$case_dir/fixture"
  fake_bin="$case_dir/bin"
  install_dir="$case_dir/install"
  env_install_dir="$case_dir/env-install"
  asset="linf-$target.tar.gz"
  mkdir -p "$fixture/linf-vtest-$target" "$fake_bin"
  printf '#!/bin/sh\nprintf "linf test\\n"\n' > "$fixture/linf-vtest-$target/linf"
  chmod 0755 "$fixture/linf-vtest-$target/linf"
  (
    cd "$fixture"
    tar -czf "$asset" "linf-vtest-$target"
    if command -v shasum >/dev/null 2>&1; then
      shasum -a 256 "$asset" > "$asset.sha256"
    else
      sha256sum "$asset" > "$asset.sha256"
    fi
  )

  cat > "$fake_bin/uname" <<EOF
#!/bin/sh
case "\$1" in
  -s) printf '%s\n' '$os' ;;
  -m) printf '%s\n' '$machine' ;;
  *) exit 1 ;;
esac
EOF
  chmod 0755 "$fake_bin/uname"

  cat > "$fake_bin/curl" <<'EOF'
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
  *"$LINF_INSTALL_ASSET.sha256")
    cp "$LINF_INSTALL_FIXTURE/$LINF_INSTALL_ASSET.sha256" "$out"
    ;;
  *"$LINF_INSTALL_ASSET")
    cp "$LINF_INSTALL_FIXTURE/$LINF_INSTALL_ASSET" "$out"
    ;;
  *)
    printf 'unexpected installer URL for %s: %s\n' "$LINF_INSTALL_ASSET" "$url" >&2
    exit 1
    ;;
esac
EOF
  chmod 0755 "$fake_bin/curl"

  output="$(
    PATH="$fake_bin:$PATH" \
    LINF_INSTALL_FIXTURE="$fixture" \
    LINF_INSTALL_ASSET="$asset" \
    LINF_INSTALL_DIR="$env_install_dir" \
    sh "$ROOT/scripts/release-install.sh" --install-dir "$install_dir"
  )"

  [ -x "$install_dir/linf" ] || { printf '%s %s: linf was not installed\n' "$os" "$machine" >&2; exit 1; }
  [ ! -e "$env_install_dir/linf" ] || { printf '%s %s: --install-dir must win over LINF_INSTALL_DIR\n' "$os" "$machine" >&2; exit 1; }
  case "$output" in
    *"Downloading linf for $target"*) ;;
    *)
      printf '%s %s: expected target %s, got:\n%s\n' "$os" "$machine" "$target" "$output" >&2
      exit 1
      ;;
  esac
  case "$output" in
    *'설치 확인: linf --version'*'Docker 확인: linf doctor'*'터미널 앱 열기: linf'*'agent에서 사용: linf skill install'*)
      ;;
    *)
      printf 'missing post-install guidance:\n%s\n' "$output" >&2
      exit 1
      ;;
  esac
  case "$output" in
    *'export PATH="'*':$PATH"'*)
      ;;
    *)
      printf 'missing PATH guidance:\n%s\n' "$output" >&2
      exit 1
      ;;
  esac
}

platform_case Darwin arm64 aarch64-apple-darwin
platform_case Darwin x86_64 x86_64-apple-darwin
platform_case Linux x86_64 x86_64-unknown-linux-gnu
platform_case Linux aarch64 aarch64-unknown-linux-gnu
platform_case Linux arm64 aarch64-unknown-linux-gnu

# A corrupted archive must fail the checksum step and install nothing.
corrupt="$TMP/Linux-x86_64/fixture"
printf 'tampered\n' >> "$corrupt/linf-x86_64-unknown-linux-gnu.tar.gz"
if PATH="$TMP/Linux-x86_64/bin:$PATH" \
   LINF_INSTALL_FIXTURE="$corrupt" \
   LINF_INSTALL_ASSET="linf-x86_64-unknown-linux-gnu.tar.gz" \
   sh "$ROOT/scripts/release-install.sh" --install-dir "$TMP/corrupt-install" >/dev/null 2>"$TMP/corrupt.err"; then
  printf 'a tampered archive was installed\n' >&2
  exit 1
fi
[ ! -e "$TMP/corrupt-install/linf" ] || { printf 'a tampered archive left a binary behind\n' >&2; exit 1; }

# An unsupported pair must fail loudly instead of guessing an archive.
mkdir -p "$TMP/unsupported"
cat > "$TMP/unsupported/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf 'Linux\n' ;;
  -m) printf 'riscv64\n' ;;
  *) exit 1 ;;
esac
EOF
chmod 0755 "$TMP/unsupported/uname"
if PATH="$TMP/unsupported:$PATH" sh "$ROOT/scripts/release-install.sh" --install-dir "$TMP/unsupported/install" 2>"$TMP/unsupported/err"; then
  printf 'unsupported platform was accepted\n' >&2
  exit 1
fi
grep -q 'unsupported platform: Linux riscv64' "$TMP/unsupported/err"
