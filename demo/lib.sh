# Shared helpers for the POSIX demo scripts: run.sh, watch.sh, stop.sh, worker-slot.sh and replay.sh.
# This file is sourced, not run. The caller sets SCRIPT_DIR to the demo folder first.
# Needs sh, curl, jq, git, ps and pgrep. Nothing here prints a token or a provider key.

REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
# Every take gets its own folder under this root. The default is under target/, which git ignores.
RUN_ROOT=${ACAN_RUN_ROOT:-$REPO_ROOT/target/acan-run}
# Cargo's target folder. CARGO_TARGET_DIR moves it, as it does for cargo.
# shellcheck disable=SC2034 # read by run.sh, which sources this file
TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/target}

die() {
  printf '%s\n' "$*" >&2
  exit 1
}

require_tools() {
  for demo_tool in "$@"; do
    if ! command -v "$demo_tool" >/dev/null 2>&1; then
      die "$demo_tool is needed and was not found on PATH"
    fi
  done
}

is_whole_number() {
  case $1 in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

# Prints the folder of the latest take, or nothing before the first take.
current_run_dir() {
  if [ -f "$RUN_ROOT/current.txt" ]; then
    sed -n '1p' "$RUN_ROOT/current.txt"
  fi
}

set_current_run_dir() {
  mkdir -p "$RUN_ROOT"
  printf '%s\n' "$1" > "$RUN_ROOT/current.txt"
}

# Prints the owner's token from a run's tokens file.
owner_token() {
  jq -r '[.[] | select(.principal.kind == "member")][0].token // empty' "$1/.acan/tokens.json"
}

# Prints one agent's token from a run's tokens file. $1 is the run folder, $2 the agent id.
agent_token() {
  jq -r --arg id "$2" '[.[] | select(.principal.kind == "agent" and .principal.id == $id)][0].token // empty' "$1/.acan/tokens.json"
}

# A fresh token: the acan_ prefix and 32 hex digits, as the PowerShell scripts make them.
new_token() {
  printf 'acan_%s\n' "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
}

# The start time of a process as ps prints it. Empty when no such process exists.
process_start() {
  ps -o lstart= -p "$1" 2>/dev/null | sed 's/^ *//; s/ *$//'
}

# True only when the pid is still alive and started at the recorded time, so a reused pid is not mistaken for it.
# The watch lock uses this.
process_alive() {
  [ -n "$2" ] && [ "$(process_start "$1")" = "$2" ]
}

# A stamp that identifies one process for as long as it runs: its start time in clock ticks since boot, read
# from /proc. Ticks do not move when the wall clock is set, which the wall-clock start time from ps does. Where
# /proc is missing, the start time as ps prints it. Prints nothing when the process is gone or a zombie.
process_stamp() {
  if [ -r "/proc/$1/stat" ]; then
    sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | awk '$1 != "Z" { print "ticks " $20 }'
  else
    stamp_start=$(process_start "$1")
    if [ -n "$stamp_start" ]; then
      printf 'lstart %s\n' "$stamp_start"
    fi
  fi
}

# True only when the pid is still the process the pid file recorded, by its stamp.
process_is() {
  [ -n "$2" ] && [ "$(process_stamp "$1")" = "$2" ]
}

# Records a started process in the run state. $1 state file, $2 name, $3 pid.
record_process() {
  jq --arg name "$2" --argjson pid "$3" --arg lstart "$(process_start "$3")" \
    '.processes += [{name: $name, pid: $pid, lstart: $lstart}]' "$1" > "$1.tmp" && mv "$1.tmp" "$1"
}

# Starts a detached process with its output in the logs folder and records it in the run state. It also writes
# the pid file that stop.sh stops it by: the pid, then the process's stamp.
# Arguments: state file, logs folder, name, working folder, then the command and its arguments.
start_logged() {
  start_state=$1
  start_logs=$2
  start_name=$3
  start_dir=$4
  shift 4
  (cd "$start_dir" && exec nohup "$@") \
    > "$start_logs/$start_name.out.log" 2> "$start_logs/$start_name.err.log" < /dev/null &
  start_pid=$!
  record_process "$start_state" "$start_name" "$start_pid"
  printf '%s %s\n' "$start_pid" "$(process_stamp "$start_pid")" > "$start_logs/$start_name.pid"
}

# Sends a signal to a process and then to its children, children first. This is what taskkill /T does on Windows.
signal_tree() {
  pgrep -P "$1" 2>/dev/null | while read -r signal_child; do
    signal_tree "$signal_child" "$2"
  done
  kill "-$2" "$1" 2>/dev/null
  return 0
}

# Stops one process tree from its pid file: $1 name, $2 pid, $3 stamp. acan gets SIGINT, so it shuts down
# cleanly. Everything else gets SIGTERM. A process still alive after 10 seconds gets SIGKILL.
stop_recorded() {
  stop_name=$1
  stop_pid=$2
  stop_stamp=$3
  if [ -z "$stop_stamp" ]; then
    if kill -0 "$stop_pid" 2>/dev/null; then
      printf 'left %s (pid %s) running: its pid file has no stamp to check it by\n' "$stop_name" "$stop_pid"
    else
      printf '%s was not running\n' "$stop_name"
    fi
    return 0
  fi
  if ! process_is "$stop_pid" "$stop_stamp"; then
    printf '%s was not running\n' "$stop_name"
    return 0
  fi
  if [ "$stop_name" = acan-serve ]; then
    stop_sig=INT
  else
    stop_sig=TERM
  fi
  signal_tree "$stop_pid" "$stop_sig"
  stop_waited=0
  while process_is "$stop_pid" "$stop_stamp" && [ "$stop_waited" -lt 10 ]; do
    sleep 1
    stop_waited=$((stop_waited + 1))
  done
  if process_is "$stop_pid" "$stop_stamp"; then
    signal_tree "$stop_pid" KILL
    printf 'stopped %s (pid %s) with SIGKILL after it ignored SIG%s\n' "$stop_name" "$stop_pid" "$stop_sig"
  else
    printf 'stopped %s (pid %s)\n' "$stop_name" "$stop_pid"
  fi
}

# True when something answers or listens on a local port. curl exits 7 when nothing listens there.
port_busy() {
  port_rc=0
  curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$1/" >/dev/null 2>&1 || port_rc=$?
  [ "$port_rc" -ne 7 ]
}

# Waits up to $2 seconds for a 200 from URL $1. Returns 1 when the time runs out.
wait_http_200() {
  wait_left=$2
  while [ "$wait_left" -gt 0 ]; do
    if [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$1")" = 200 ]; then
      return 0
    fi
    sleep 1
    wait_left=$((wait_left - 1))
  done
  return 1
}

# Calls the acan API and prints the response body. Fails on an HTTP error, so the caller can stop.
# Arguments: port, token, method, path, and an optional JSON body.
api() {
  if [ $# -ge 5 ]; then
    curl -sf --max-time 60 -X "$3" -H "Authorization: Bearer $2" -H 'Content-Type: application/json' --data "$5" "http://127.0.0.1:$1$4"
  else
    curl -sf --max-time 60 -X "$3" -H "Authorization: Bearer $2" "http://127.0.0.1:$1$4"
  fi
}
