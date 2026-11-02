#!/bin/sh
# Installs the acan alpha on Linux and macOS.
#
# It downloads the newest release for this platform (pre-releases included), checks its SHA256
# against the release's SHA256SUMS, and puts the acan binary on PATH. It prints what you need
# next. It changes nothing else: no config, no keys, no shell profile.
#
#   curl -fsSL https://raw.githubusercontent.com/NDilanka/acan/main/install.sh | sh
#
# Environment overrides:
#   ACAN_INSTALL_TAG  install this release tag, such as v0.1.0-alpha.1, instead of the newest
#   ACAN_BIN_DIR      where the acan binary goes (default: $HOME/.local/bin)
#   ACAN_SHARE_DIR    where the web UI goes (default: $HOME/.local/share/acan)

set -eu

REPO="NDilanka/acan"

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum or shasum is required to check the download"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
      *) die "there is no Linux build for $arch yet; the alpha ships x86_64" ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) die "there is no macOS build for $arch" ;;
    esac
    echo "note: the macOS builds are built in CI but untested in the alpha."
    ;;
  *)
    die "$os is not handled here; on Windows, run install.ps1"
    ;;
esac

tag="${ACAN_INSTALL_TAG:-}"
if [ -z "$tag" ]; then
  tag="$(curl -fsSL -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases?per_page=1" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || die "no release found for $REPO (GitHub may be rate limiting; try again later)"
fi

base="https://github.com/$REPO/releases/download/$tag"
archive="acan-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading acan $tag for $target"
curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "cannot download $archive from $tag"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "cannot download SHA256SUMS from $tag"

expected="$(grep " $archive\$" "$tmp/SHA256SUMS" | cut -d' ' -f1 || true)"
[ -n "$expected" ] || die "$archive is not listed in SHA256SUMS; refusing to install"
actual="$(sha256 "$tmp/$archive")"
[ "$actual" = "$expected" ] || die "SHA256 mismatch for $archive (expected $expected, got $actual); nothing was installed"
echo "SHA256 ok: $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
stage="$tmp/acan-$target"
[ -f "$stage/acan" ] || die "the archive has no acan binary"
[ -d "$stage/web" ] || die "the archive has no web directory"

bin_dir="${ACAN_BIN_DIR:-$HOME/.local/bin}"
share_dir="${ACAN_SHARE_DIR:-$HOME/.local/share/acan}"
mkdir -p "$bin_dir" "$share_dir"
install -m 755 "$stage/acan" "$bin_dir/acan"
rm -rf "$share_dir/web"
cp -R "$stage/web" "$share_dir/web"

echo
echo "Installed acan $tag: $bin_dir/acan (web UI in $share_dir/web)"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    echo "$bin_dir is not on your PATH. Add it to your shell profile, for example:"
    echo "  export PATH=\"$bin_dir:\$PATH\""
    ;;
esac

cat <<EOF

Before you run it:
  - Git: 'git --version' must work, and your project must be a git repository with a commit.
  - Docker: 'docker version' must work. Candidate code runs in containers.
  - A model key: bring your own key for the provider you use. Keys stay on your machine and
    are never sent anywhere else by acan. Without a routing file, acan answers with labelled
    offline replies. The README explains the key setup.

Start it on your project:
  acan serve --repo /path/to/your/project

Then open the address it prints. Stop it with Ctrl+C.
EOF
