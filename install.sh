#!/bin/sh
# Installs the Acan app for this user, with no admin rights. Run it from the folder that the
# archive unpacks to:
#
#   tar -xzf acan-<version>-linux-x64.tar.gz
#   cd acan-<version>-linux-x64
#   sh install.sh
#
# It copies acan, acan-worker and web/ to ~/.local/share/acan, and links acan and acan-worker into
# ~/.local/bin. It changes nothing else: no config, no keys, no shell profile.
#
# Environment overrides:
#   ACAN_BIN_DIR    where the links go (default: $HOME/.local/bin)
#   ACAN_SHARE_DIR  where the files go (default: $HOME/.local/share/acan)

set -eu

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

here="$(cd "$(dirname "$0")" && pwd)"
[ -f "$here/acan" ] || die "no acan binary beside this script; run it from the unpacked archive"
[ -f "$here/acan-worker" ] || die "no acan-worker binary beside this script"
[ -d "$here/web" ] || die "no web folder beside this script"
[ -f "$here/FLAVOR" ] || die "no FLAVOR file beside this script"

bin_dir="${ACAN_BIN_DIR:-$HOME/.local/bin}"
share_dir="${ACAN_SHARE_DIR:-$HOME/.local/share/acan}"
mkdir -p "$bin_dir" "$share_dir"
cp "$here/acan" "$here/acan-worker" "$here/FLAVOR" "$share_dir/"
chmod 755 "$share_dir/acan" "$share_dir/acan-worker"
rm -rf "$share_dir/web"
cp -R "$here/web" "$share_dir/web"
ln -sf "$share_dir/acan" "$bin_dir/acan"
ln -sf "$share_dir/acan-worker" "$bin_dir/acan-worker"

echo "Installed acan to $share_dir, with links in $bin_dir."
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    echo "$bin_dir is not on your PATH. Add it to your shell profile, for example:"
    echo "  export PATH=\"$bin_dir:\$PATH\""
    ;;
esac

cat <<'EOF'

Before you run it:
  - Git: 'git --version' must work, and your project must be a git repository with a commit.
  - Docker: 'docker version' must work. Candidate code runs in containers.
  - Keys are optional. Without one, acan answers with labelled offline replies. Keys stay on your
    machine. The README explains the setup.

Start it on your project:
  acan serve --repo /path/to/your/project

Then open the address it prints. Stop it with Ctrl+C.
EOF
