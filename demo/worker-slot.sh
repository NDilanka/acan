#!/bin/sh
# Runs one worker slot, as worker-slot.ps1 does. An acan-worker process builds one task and exits, so the slot
# starts a new worker after each exit, up to -m runs. Each run writes its own log pair under the run's logs folder.
#
# Usage: worker-slot.sh -n name -a agent -d run_dir -b worker_binary -p port [-m max_runs] [-t wait_secs]

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh disable=SC1091
. "$SCRIPT_DIR/lib.sh"

slot_name=
agent=
run_dir=
worker=
port=
max_runs=8
wait_secs=900
while getopts 'n:a:d:b:p:m:t:h' opt; do
  case $opt in
    n) slot_name=$OPTARG ;;
    a) agent=$OPTARG ;;
    d) run_dir=$OPTARG ;;
    b) worker=$OPTARG ;;
    p) port=$OPTARG ;;
    m) max_runs=$OPTARG ;;
    t) wait_secs=$OPTARG ;;
    *) die 'usage: worker-slot.sh -n name -a agent -d run_dir -b worker_binary -p port [-m max_runs] [-t wait_secs]' ;;
  esac
done
if [ -z "$slot_name" ] || [ -z "$agent" ] || [ -z "$run_dir" ] || [ -z "$worker" ] || [ -z "$port" ]; then
  die 'worker-slot.sh needs -n, -a, -d, -b and -p'
fi

token=$(agent_token "$run_dir" "$agent")
if [ -z "$token" ]; then
  die "no agent token for $agent in $run_dir/.acan/tokens.json"
fi

logs="$run_dir/logs"
run=1
while [ "$run" -le "$max_runs" ]; do
  (cd "$run_dir" && "$worker" \
    --token "$token" \
    --server "http://127.0.0.1:$port" \
    --repo "$run_dir" \
    --llm "$run_dir/.acan/llm.toml" \
    --env "$run_dir/.env" \
    --test-command 'bun run test' \
    --wait-secs "$wait_secs" \
    --heartbeat-secs 20) \
    > "$logs/$slot_name.$run.out.log" 2> "$logs/$slot_name.$run.err.log" < /dev/null
  run=$((run + 1))
  sleep 3
done
