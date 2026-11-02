#!/bin/sh
# Follows one take through the owner's event log and writes a timeline and a summary under the take's logs
# folder. This is watch.ps1 for POSIX shells. With -s 1, 2 or 3 it settles each clash the way a click on that
# menu row would, so a take runs without the UI. With -s 0 a human settles in the UI.
#
# One watch per take. It refuses to start when the take already has an events log, and while another watch
# holds logs/watch.lock. It never truncates or overwrites a take's logs. It stops when everything has landed,
# when the timeout passes, when the server refuses the owner token, or after -m polls in a row fail. The
# summary records which of these ended the watch (watch_end).
#
# "Clash before write" means the clash was recorded before the origin cleared any task to build. The origin
# clears a task only after its declarations are free of blocking clashes, and a worker writes only after that
# clearance, so no file is written before the clash.
#
# Usage: watch.sh [-d run_dir] [-p port] [-s settle] [-t timeout_secs] [-i poll_secs] [-m max_refused]
# Needs jq and curl. The jq programs watch-lib.jq, watch-event.jq and watch-summary.jq sit next to this script.

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh disable=SC1091
. "$SCRIPT_DIR/lib.sh"

run_dir=
port=
settle=0
timeout_secs=1500
poll_secs=1
max_refused=10
while getopts 'd:p:s:t:i:m:h' opt; do
  case $opt in
    d) run_dir=$OPTARG ;;
    p) port=$OPTARG ;;
    s) settle=$OPTARG ;;
    t) timeout_secs=$OPTARG ;;
    i) poll_secs=$OPTARG ;;
    m) max_refused=$OPTARG ;;
    *) die 'usage: watch.sh [-d run_dir] [-p port] [-s settle] [-t timeout_secs] [-i poll_secs] [-m max_refused]' ;;
  esac
done

require_tools jq curl

if [ -z "$run_dir" ]; then
  run_dir=$(current_run_dir)
fi
if [ -z "$run_dir" ]; then
  die 'no run folder given and no current run; run run.sh first'
fi
state="$run_dir/logs/run.json"
if [ ! -f "$state" ]; then
  die "no run state in $run_dir; run run.sh first"
fi
if [ -z "$port" ]; then
  port=$(jq -r '.port // 7311' "$state")
fi
for number in "$port" "$timeout_secs" "$poll_secs" "$max_refused"; do
  if ! is_whole_number "$number"; then
    die "watch.sh takes whole numbers for the port, timeout, poll interval and refused-poll limit; got $number"
  fi
done
case $settle in
  0 | 1 | 2 | 3) ;;
  *) die "settle must be 0, 1, 2 or 3; got $settle" ;;
esac

owner=$(owner_token "$run_dir")
if [ -z "$owner" ]; then
  die "no owner token in $run_dir/.acan/tokens.json"
fi

logs="$run_dir/logs"
timeline="$logs/timeline.txt"
raw="$logs/events.jsonl"
summary="$logs/summary.json"
lock="$logs/watch.lock"
serve_err="$logs/acan-serve.err.log"

# A take is watched once. A second watch must not truncate or overwrite the first one's logs.
if [ -f "$raw" ]; then
  die "$run_dir already has a watch ($raw). Start a new take with run.sh instead."
fi
if [ -f "$lock" ]; then
  held_pid=$(jq -r '.pid // empty' "$lock")
  held_start=$(jq -r '.lstart // empty' "$lock")
  if [ -n "$held_pid" ] && process_alive "$held_pid" "$held_start"; then
    die "a watcher (pid $held_pid) already follows $run_dir"
  fi
fi
tmp_dir=$(mktemp -d) || die 'could not make a temporary folder'
batch="$tmp_dir/batch.json"
records="$tmp_dir/records.ndjson"
settle_out="$tmp_dir/settle.out"
printf '{"pid":%s,"lstart":"%s"}\n' "$$" "$(process_start "$$")" > "$lock"
# The trap is set only once this watch holds the lock, so a refused start never removes another watch's lock.
trap 'rm -rf "$tmp_dir"; rm -f "$lock"' EXIT
trap 'exit 130' INT TERM HUP
: > "$timeline"
: > "$raw"

# The timeline gets each line, and so does the terminal.
say() {
  printf '%s\n' "$1" >> "$timeline"
  printf '%s\n' "$1"
}

# Fetches the events from seq $2 on into file $3 and prints the HTTP status, 000 when no answer came back.
fetch_events() {
  curl -s --max-time 60 -o "$3" -w '%{http_code}' \
    -H "Authorization: Bearer $1" "http://127.0.0.1:$port/api/events?from=$2"
}

# Sends the settle command for one clash record ($1 is the record) and says what came back.
settle_record() {
  settle_clash=$(printf '%s\n' "$1" | jq -r '.settle.clash')
  settle_choice=$(printf '%s\n' "$1" | jq -c '.settle.choice')
  case " $settled " in
    *" $settle_clash "*) return 0 ;;
  esac
  settled="$settled$settle_clash "
  body=$(jq -cn --argjson clash "$settle_clash" --argjson choice "$settle_choice" \
    '{cmd: "settle_clash", clash: $clash, choice: $choice}')
  code=$(curl -s -o "$settle_out" -w '%{http_code}' --max-time 60 -X POST \
    -H "Authorization: Bearer $owner" -H 'Content-Type: application/json' \
    --data "$body" "http://127.0.0.1:$port/api/commands")
  case $code in
    2??) say "          settled c-$settle_clash with option $settle as a human would: $settle_choice" ;;
    *) say "          settle of c-$settle_clash refused: $(cat "$settle_out" 2>/dev/null)" ;;
  esac
}

# Writes the events of one good poll to the raw log, says each one, and settles clashes when asked.
# Sets origin when the order is posted and next to the seq after the last event.
process_batch() {
  count=$(jq -r '(. // []) | length' "$batch")
  if [ "$count" = 0 ]; then
    return 0
  fi
  jq -c '(. // [])[]' "$batch" >> "$raw"
  if ! jq -L "$SCRIPT_DIR" -c --argjson origin "${origin:-null}" --argjson settle "$settle" \
    -f "$SCRIPT_DIR/watch-event.jq" "$batch" > "$records"; then
    die 'could not format the events of a poll'
  fi
  while IFS= read -r record; do
    say "$(printf '%s\n' "$record" | jq -r '.line')"
    case $record in
      *'"settle":null'*) ;;
      *) settle_record "$record" ;;
    esac
  done < "$records"
  if [ -z "$origin" ]; then
    origin=$(jq -r '[(. // [])[] | select(.event.type == "order_posted") | .at][0] // empty' "$batch")
  fi
  next=$(jq -r '(. // []) | map(.seq) | max + 1' "$batch")
}

# The worker logs one line per write, stamped when the write returned. Those stamps are the only record of a
# write in the run, since the origin logs a clearance, not a write. Numbered worker logs (worker-1.2.err.log)
# match the same glob. Prints a JSON array of {utc, path, task}.
collect_writes() {
  for worker_log in "$logs"/worker-*.err.log; do
    [ -f "$worker_log" ] || continue
    jq -R -c '
      select(test("wrote \\S+ on t-[0-9]+") and test("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z"))
      | capture("wrote (?<path>\\S+) on (?<task>t-[0-9]+)") as $w
      | capture("(?<utc>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z)") as $u
      | {utc: ($u.utc | capture("^(?<base>[^.]+)(\\.(?<frac>[0-9]*))?Z$")
               | "\(.base).\(((.frac // "") + "0000000")[0:7])Z"),
         path: $w.path, task: $w.task}
    ' "$worker_log"
  done | jq -s '.'
}

settled=' '
origin=
next=0
refused=0
watch_end=timeout
if [ "$settle" = 0 ]; then
  settle_label='by hand in the UI'
else
  settle_label=$settle
fi
say "watching $run_dir (settle option: $settle_label, timeout ${timeout_secs}s)"

deadline=$(($(date +%s) + timeout_secs))
while [ "$(date +%s)" -lt "$deadline" ]; do
  code=$(fetch_events "$owner" "$next" "$batch")
  ok=0
  case $code in
    401 | 403)
      say "event poll refused the owner token (HTTP $code); this watch stops"
      watch_end=token_refused
      break
      ;;
    2??)
      if jq -e '. == null or type == "array"' "$batch" > /dev/null 2>&1; then
        ok=1
      fi
      ;;
  esac
  if [ "$ok" = 1 ]; then
    refused=0
    process_batch
    progress=$(jq -s -r '
      ([.[] | select(.event.type == "order_split")] | last | (.event.tasks // [] | length)) as $tasks
      | ([.[] | select(.event.type == "landed") | .event.task | tostring] | unique | length) as $landed
      | "\($tasks) \($landed)"' "$raw")
    # shellcheck disable=SC2086
    set -- $progress
    task_total=$1
    landed_total=$2
    if [ "$task_total" -gt 0 ] && [ "$landed_total" -eq "$task_total" ]; then
      watch_end=all_landed
      break
    fi
  else
    refused=$((refused + 1))
    if [ "$code" = 000 ]; then
      poll_reason="no answer on port $port"
    else
      poll_reason="HTTP $code"
    fi
    say "event poll failed ($refused in a row): $poll_reason"
    if [ "$refused" -ge "$max_refused" ]; then
      say "$refused polls in a row failed; the server is gone, so this watch stops"
      watch_end=server_gone
      break
    fi
  fi
  sleep "$poll_secs"
done

# The summary is computed from the stored events, so it matches the raw log.
writes=$(collect_writes)
usage=$(api "$port" "$owner" GET /api/usage 2> /dev/null) || usage=
if ! printf '%s' "$usage" | jq -e . > /dev/null 2>&1; then
  usage=null
fi
live_failures=0
if [ -f "$serve_err" ]; then
  live_failures=$(grep -c 'live model call failed' "$serve_err" || true)
fi
jq -L "$SCRIPT_DIR" -s -f "$SCRIPT_DIR/watch-summary.jq" \
  --argjson writes "$writes" --argjson usage "$usage" \
  --argjson live_failures "$live_failures" --arg watch_end "$watch_end" "$raw" > "$summary"
say "$(jq -r '"OUTCOME: \(.outcome) (watch ended: \(.watch_end); tasks \(.tasks), landed \(.landed), clashes \(.counts.clashes), live call failures \(.live_call_failures))"' "$summary")"
cat "$summary"
