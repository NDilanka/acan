#!/bin/sh
# Stops every process the take started, and only those, as stop.ps1 does. Each process is matched by pid and
# start time, so a pid the OS has reused is left alone. With no -d it stops the latest take.
# acan gets SIGINT, so it shuts down cleanly. Every other process gets SIGTERM, and SIGKILL if it is still
# alive after 10 seconds.
#
# Usage: stop.sh [-d run_dir]

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh disable=SC1091
. "$SCRIPT_DIR/lib.sh"

run_dir=
while getopts 'd:h' opt; do
  case $opt in
    d) run_dir=$OPTARG ;;
    *) die 'usage: stop.sh [-d run_dir]' ;;
  esac
done

require_tools jq ps pgrep

if [ -z "$run_dir" ]; then
  run_dir=$(current_run_dir)
fi
if [ -z "$run_dir" ]; then
  printf '%s\n' 'no run folder given and no current run; nothing to stop'
  exit 0
fi
state="$run_dir/logs/run.json"
if [ ! -f "$state" ]; then
  printf 'no run state in %s; nothing to stop\n' "$run_dir"
  exit 0
fi

tab=$(printf '\t')
jq -r '.processes[] | [.name, (.pid | tostring), .lstart] | join("\t")' "$state" |
  while IFS=$tab read -r name pid lstart; do
    stop_recorded "$name" "$pid" "$lstart"
  done
