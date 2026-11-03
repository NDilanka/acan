#!/bin/sh
# Starts the Acan Demo on this machine.
#
#   sh start-acan-demo.sh            the default mode
#   sh start-acan-demo.sh replay     the recorded run. No keys needed.
#   sh start-acan-demo.sh live       the live page, with the owner's link. No keys needed for the offline replies.
#
# Each start clones the bundled demo repository into a new folder under
# ~/.local/share/acan-demo/runs, so the bundled copy is never changed. The server runs in the
# background and logs beside the copy. The script prints the command that stops it.
# The default is replay until the offline scope draft ships, then live.

set -eu

# Change to 'live' once the offline scope draft is in the build.
default_mode=replay

die() {
  printf 'start-acan-demo.sh: %s\n' "$1" >&2
  exit 1
}

mode="${1:-default}"
if [ "$mode" = default ]; then mode="$default_mode"; fi
case "$mode" in
  replay | live) ;;
  *) die "unknown mode $mode; use replay or live" ;;
esac

command -v git >/dev/null 2>&1 || die "Acan needs Git. Install it with your package manager, then start the demo again."
command -v curl >/dev/null 2>&1 || die "this launcher needs curl"

app="$(cd "$(dirname "$0")" && pwd)"
[ -f "$app/acan-demo.bundle" ] || die "no acan-demo.bundle beside this script"
port="${ACAN_DEMO_PORT:-7311}"
runs="${XDG_DATA_HOME:-$HOME/.local/share}/acan-demo/runs"
stamp="$(date +%Y%m%d-%H%M%S)"
repo="$runs/$stamp"
log="$runs/$stamp.log"
mkdir -p "$runs"
# The bundle carries main only, so the clone names it. Without --branch the copy comes out empty.
git clone --quiet --branch main -- "$app/acan-demo.bundle" "$repo"

"$app/acan" serve --repo "$repo" --port "$port" >"$log" 2>&1 &
pid=$!
base="http://127.0.0.1:$port"
tries=0
until curl -fsS -o /dev/null "$base/" 2>/dev/null; do
  tries=$((tries + 1))
  if [ "$tries" -gt 120 ] || ! kill -0 "$pid" 2>/dev/null; then
    die "the server did not start; its log is $log"
  fi
  sleep 0.5
done

if [ "$mode" = live ]; then
  # The server writes each entry as {"token":"...","principal":{"kind":"member","id":"owner"}}.
  token="$(tr -d ' \n\t' <"$repo/.acan/tokens.json" | sed -n 's/.*"token":"\([^"]*\)","principal":{"kind":"member","id":"owner"}.*/\1/p')"
  [ -n "$token" ] || die "the owner token is not in $repo/.acan/tokens.json"
  url="$base/#token=$token"
else
  url="$base/?replay=1"
fi

if command -v xdg-open >/dev/null 2>&1; then
  xdg-open "$url" >/dev/null 2>&1 || true
fi
echo "Acan Demo is running in $mode mode at $base"
echo "Demo copy: $repo"
echo "Log: $log"
echo "Stop the server with: kill $pid"
echo "If the browser did not open, go to: $url"
