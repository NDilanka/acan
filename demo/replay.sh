#!/bin/sh
# Replays the recorded provider receipts in receipts/ without any API key. This is replay.ps1 for POSIX shells.
#
# It checks each receipt against receipts/SHA256SUMS, then prints the receipts and where the recorded run
# plays. It reads no keys and makes no network calls, so a judge can run it on a machine with no provider
# accounts.
#
# Usage: replay.sh [-d receipts_dir] [-v]
#   -d  the folder with the receipts and SHA256SUMS (default: receipts/ at the repository root)
#   -v  check the checksums only; without it the receipts are printed after the check
# Exit code 0 when every receipt matches its checksum, 1 otherwise.

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
receipts_dir="$REPO_ROOT/receipts"
verify_only=0
while getopts 'd:vh' opt; do
  case $opt in
    d) receipts_dir=$OPTARG ;;
    v) verify_only=1 ;;
    *) printf '%s\n' 'usage: replay.sh [-d receipts_dir] [-v]' >&2; exit 1 ;;
  esac
done

sums="$receipts_dir/SHA256SUMS"
if [ ! -f "$sums" ]; then
  printf 'missing checksum file: %s\n' "$sums"
  exit 1
fi

# Prints the SHA-256 of a file as lowercase hex.
sha256_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Trims the spaces and tabs at both ends of $1.
trim() {
  printf '%s' "$1" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

failures=0
names=
while IFS= read -r line || [ -n "$line" ]; do
  trimmed=$(trim "$line")
  if [ -z "$trimmed" ]; then
    continue
  fi
  expected=$(printf '%s' "${trimmed%%[[:space:]]*}" | tr 'A-F' 'a-f')
  name=${trimmed#"${trimmed%%[[:space:]]*}"}
  name=$(trim "$name")
  names="$names$name
"
  path="$receipts_dir/$name"
  if [ ! -f "$path" ]; then
    printf 'MISSING   %s\n' "$name"
    failures=$((failures + 1))
    continue
  fi
  actual=$(sha256_of "$path")
  if [ "$actual" = "$expected" ]; then
    printf 'OK        %s\n' "$name"
  else
    printf 'MISMATCH  %s\n' "$name"
    failures=$((failures + 1))
  fi
done < "$sums"

if [ "$failures" -gt 0 ]; then
  printf '%s receipt(s) failed the check. Do not use them.\n' "$failures"
  exit 1
fi
count=$(printf '%s' "$names" | grep -c .)
printf 'all %s receipts match SHA256SUMS\n' "$count"

if [ "$verify_only" = 0 ]; then
  rule=$(printf '%078d' 0 | tr 0 =)
  printf '%s' "$names" | while IFS= read -r name; do
    printf '\n%s\n' "$rule"
    printf 'receipt: %s\n' "$name"
    printf '%s\n' "$rule"
    cat "$receipts_dir/$name"
  done
fi

# Where the recorded run plays. Neither route needs a key.
replay_url='https://ndilanka.github.io/acan/?replay=1'
printf '\nRecorded run, no keys needed:\n'
printf '  hosted: %s\n' "$replay_url"
printf '%s\n' '  local:  build the UI once with crates/acan-render-web/build.ps1'
printf '%s\n' '          copy replay/ into crates/acan-render-web/www/'
printf '%s\n' '          cargo run -p acan-server -- serve --repo . --web crates/acan-render-web/www'
printf '%s\n' '          then open http://127.0.0.1:7311/?replay=1'

exit 0
