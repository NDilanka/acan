#!/bin/sh
# Stops every process the take started, and only those, as stop.ps1 does. Each process is stopped by the pid file
# that start_logged wrote for it in the run's logs folder. The pid file holds the pid and the stamp the process had
# when it started, so a pid the OS has reused is left alone. With no -d it stops the latest take.
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

require_tools ps pgrep

if [ -z "$run_dir" ]; then
  run_dir=$(current_run_dir)
fi
if [ -z "$run_dir" ]; then
  printf '%s\n' 'no run folder given and no current run; nothing to stop'
  exit 0
fi
logs="$run_dir/logs"
if [ ! -d "$logs" ]; then
  printf 'no logs in %s; nothing to stop\n' "$run_dir"
  exit 0
fi

found=0
for pid_file in "$logs"/*.pid; do
  if [ ! -f "$pid_file" ]; then
    continue
  fi
  found=1
  stop_pid=
  stop_stamp=
  read -r stop_pid stop_stamp < "$pid_file"
  stop_recorded "$(basename "$pid_file" .pid)" "$stop_pid" "$stop_stamp"
done
if [ "$found" = 0 ]; then
  printf 'no pid files in %s; nothing to stop\n' "$logs"
fi
