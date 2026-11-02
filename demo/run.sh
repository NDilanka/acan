#!/bin/sh
# Stages one take of the hackathon demo on a fresh copy of the demo repository. This is run.ps1 for POSIX
# shells (Linux and macOS). The steps and their order are the same as run.ps1.
#
# Each take gets its own run folder, <run root>/take-<stamp>, and current.txt names it. The run root is
# target/acan-run in this checkout, or ACAN_RUN_ROOT when that is set. A take never reuses a folder, so an
# earlier take's logs are never overwritten, and a stale process from an earlier take writes only into that
# take's folder.
#
# It clones the demo repo into the run folder, copies the routing file and the provider key file in, starts
# acan serve with the UI and the demo app, mints tokens for the owner and one agent per worker, registers the
# agents, posts the order with its witnesses, and starts one worker slot per agent (worker-slot.sh). It returns
# once everything is running. Run watch.sh next to follow the take.
#
# The owner token is never printed. It is in the run folder, in .acan/tokens.json and in logs/ui-url.txt,
# which holds the UI URL with the token in its fragment.
#
# Usage: run.sh [-d run_dir] [-R demo_repo] [-p port] [-P demo_port] [-m machine] [-n workers]
#               [-s sentence] [-t wait_secs] [-S settle] [-w]
#   -d  run folder (default: a new take folder under the run root)
#   -R  demo repo to clone (default: ACAN_DEMO_REPO, then the public demo repo; a local path works too)
#   -p  port of acan serve (default 7311)     -P  port of the demo app (default 7312)
#   -m  machine the order is posted on (default middleware)
#   -n  worker slots, one agent each (default 3)
#   -t  seconds a worker waits for work (default 900)
#   -S  settle option that watch.sh uses with -w (default 3)
#   -w  follow the take in this process until it ends
#
# Environment: ACAN_RUN_ROOT, ACAN_DEMO_REPO, ACAN_LLM_FILE (default crates/acan-llm/llm.toml),
# ACAN_ENV_FILE (default crates/acan-llm/.env, the provider keys, copied into the run folder and never printed),
# CARGO_TARGET_DIR (default target/).

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh disable=SC1091
. "$SCRIPT_DIR/lib.sh"

run_dir=
demo_repo=${ACAN_DEMO_REPO:-}
port=7311
demo_port=7312
machine=middleware
workers=3
sentence='Add per-user rate limiting and an audit log of every API request, and keep the login flow untouched.'
wait_secs=900
settle=3
watch_take=0
llm_src=${ACAN_LLM_FILE:-$REPO_ROOT/crates/acan-llm/llm.toml}
env_src=${ACAN_ENV_FILE:-$REPO_ROOT/crates/acan-llm/.env}

usage='usage: run.sh [-d run_dir] [-R demo_repo] [-p port] [-P demo_port] [-m machine] [-n workers] [-s sentence] [-t wait_secs] [-S settle] [-w]'
while getopts 'd:R:p:P:m:n:s:t:S:wh' opt; do
  case $opt in
    d) run_dir=$OPTARG ;;
    R) demo_repo=$OPTARG ;;
    p) port=$OPTARG ;;
    P) demo_port=$OPTARG ;;
    m) machine=$OPTARG ;;
    n) workers=$OPTARG ;;
    s) sentence=$OPTARG ;;
    t) wait_secs=$OPTARG ;;
    S) settle=$OPTARG ;;
    w) watch_take=1 ;;
    *) die "$usage" ;;
  esac
done

for number in "$port" "$demo_port" "$workers" "$wait_secs"; do
  if ! is_whole_number "$number"; then
    die "ports, workers and wait_secs take whole numbers; got $number"
  fi
done
if [ "$workers" -lt 1 ]; then
  die 'run.sh needs at least one worker slot (-n)'
fi
case $settle in
  0 | 1 | 2 | 3) ;;
  *) die "settle must be 0, 1, 2 or 3; got $settle" ;;
esac
if [ -z "$demo_repo" ]; then
  demo_repo=https://github.com/NDilanka/acan-demo.git
fi

require_tools git cargo jq curl bun nohup

# A failed take leaves its started processes running, as run.ps1 does, so the message names the stop command.
fail_take() {
  printf '%s\n' "$1" >&2
  if [ -n "$run_dir" ] && [ -f "$run_dir/logs/run.json" ]; then
    printf 'processes started so far are still running; stop them with: sh %s/stop.sh -d "%s"\n' "$SCRIPT_DIR" "$run_dir" >&2
  fi
  exit 1
}

# Stops the take before anything starts when a port is taken, and names the parameter to change.
assert_port_free() {
  if port_busy "$1"; then
    fail_take "port $1 is already in use. Stop what holds it, or start the take on a free port with $2, for example $(($1 + 101))."
  fi
}

# Set the state field $2 to the string $3 in the run state file $1.
set_state() {
  jq --arg k "$2" --arg v "$3" '.[$k] = $v' "$1" > "$1.tmp" && mv "$1.tmp" "$1"
}

# The previous take's processes stop. Its folder stays as it was.
previous=$(current_run_dir)
if [ -n "$previous" ]; then
  sh "$SCRIPT_DIR/stop.sh" -d "$previous"
fi

if [ -z "$run_dir" ]; then
  run_dir="$RUN_ROOT/take-$(date +%Y%m%d-%H%M%S)"
fi
if [ -e "$run_dir" ]; then
  die "$run_dir already exists; a take never reuses a folder"
fi
# Both ports are checked before the clone and the build, so a busy port costs nothing.
assert_port_free "$port" '-p'
assert_port_free "$demo_port" '-P'

mkdir -p "$(dirname "$run_dir")"
if ! git clone --quiet "$demo_repo" "$run_dir"; then
  die "git clone of $demo_repo failed"
fi
set_current_run_dir "$run_dir"

logs="$run_dir/logs"
mkdir -p "$logs"
state="$logs/run.json"
jq -n --arg runDir "$run_dir" --arg startedUtc "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --argjson port "$port" --argjson demoPort "$demo_port" --arg machine "$machine" \
  --argjson workers "$workers" --arg sentence "$sentence" \
  '{runDir: $runDir, startedUtc: $startedUtc, port: $port, demoPort: $demoPort, machine: $machine,
    workers: $workers, sentence: $sentence, processes: []}' > "$state"

# The copied key file must not show up as untracked in the run repo, since that blocks a land.
mkdir -p "$run_dir/.git/info"
printf '%s\n' '/.env' >> "$run_dir/.git/info/exclude"

acan_dir="$run_dir/.acan"
mkdir -p "$acan_dir"
if [ ! -f "$llm_src" ]; then
  fail_take "no routing file at $llm_src; set ACAN_LLM_FILE to one"
fi
if [ ! -f "$env_src" ]; then
  fail_take "no provider key file at $env_src; set ACAN_ENV_FILE to one (an empty file runs without keys)"
fi
cp "$llm_src" "$acan_dir/llm.toml"
cp "$env_src" "$run_dir/.env"
chmod 600 "$run_dir/.env"

# Tokens go in before the first start, so the server loads them and mints no owner token of its own.
owner=$(new_token)
tokens=$(jq -n --arg owner "$owner" '[{token: $owner, principal: {kind: "member", id: "owner"}}]')
agents=
i=1
while [ "$i" -le "$workers" ]; do
  agent="agent-$i"
  agents="$agents $agent"
  agent_tok=$(new_token)
  tokens=$(printf '%s\n' "$tokens" \
    | jq --arg t "$agent_tok" --arg id "$agent" '. + [{token: $t, principal: {kind: "agent", id: $id}}]')
  i=$((i + 1))
done
printf '%s\n' "$tokens" > "$acan_dir/tokens.json"
chmod 600 "$acan_dir/tokens.json"

if ! (cd "$REPO_ROOT" && cargo build -p acan-server -p acan-agent --bins) > "$logs/build.log" 2>&1; then
  fail_take "cargo build failed; see $logs/build.log"
fi
acan_bin="$TARGET_DIR/debug/acan"
worker_bin="$TARGET_DIR/debug/acan-worker"
for built in "$acan_bin" "$worker_bin"; do
  if [ ! -x "$built" ]; then
    fail_take "cargo build did not produce $built"
  fi
done
www="$REPO_ROOT/crates/acan-render-web/www"

start_logged "$state" "$logs" acan-serve "$run_dir" "$acan_bin" serve \
  --repo "$run_dir" \
  --port "$port" \
  --web "$www" \
  --llm "$acan_dir/llm.toml" \
  --protected-branch main \
  --witness-http-base-url "http://127.0.0.1:$demo_port" \
  --witness-concurrency 2
if ! wait_http_200 "http://127.0.0.1:$port/health" 60; then
  fail_take "acan serve did not answer on port $port; see $logs/acan-serve.err.log"
fi

bun_bin=$(command -v bun)
PORT=$demo_port
export PORT
start_logged "$state" "$logs" demo-app "$run_dir" "$bun_bin" src/server.ts
unset PORT
if ! wait_http_200 "http://127.0.0.1:$demo_port/health" 60; then
  fail_take "the demo app did not answer on port $demo_port; see $logs/demo-app.err.log"
fi

plant=$(api "$port" "$owner" GET /api/plant) || fail_take "could not read the plant from port $port"
echo "plant machines: $(printf '%s\n' "$plant" | jq -r '[.machines[].name] | join(", ")')"
if ! printf '%s\n' "$plant" | jq -e --arg m "$machine" 'any(.machines[]; .name == $m)' > /dev/null; then
  fail_take "machine '$machine' is not on the plant; pick one of the names above and pass -m"
fi

for agent in $agents; do
  body=$(jq -cn --arg a "$agent" '{cmd: "register_agent", agent: $a}')
  api "$port" "$owner" POST /api/commands "$body" > /dev/null || fail_take "register_agent failed for $agent"
done

# The witnesses are the bar the build must clear: the login flow, the links flow, and the health route.
order=$(jq -cn --arg m "$machine" --arg s "$sentence" '{
  cmd: "post_order",
  machine: $m,
  sentence: $s,
  witnesses: [
    {check: {kind: "test", command: "bun test test/login.test.ts"}, reason: "the login flow stays untouched"},
    {check: {kind: "test", command: "bun test test/links.test.ts"}, reason: "links keep working"},
    {check: {kind: "http", method: "GET", path: "/health", expect_status: 200, expect_body: "ok"},
     reason: "the app still answers its health check"}
  ]
}')
posted=$(api "$port" "$owner" POST /api/commands "$order") || fail_take 'post_order failed'
posted_count=$(printf '%s\n' "$posted" | jq -r 'if type == "array" then length else 1 end')
set_state "$state" orderPostedUtc "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "order posted: $posted_count event(s) returned"

# Each slot runs acan-worker again after every task, so the slots can build a split of any size.
i=1
while [ "$i" -le "$workers" ]; do
  start_logged "$state" "$logs" "worker-$i" "$run_dir" \
    sh "$SCRIPT_DIR/worker-slot.sh" -n "worker-$i" -a "agent-$i" -d "$run_dir" \
    -b "$worker_bin" -p "$port" -t "$wait_secs"
  i=$((i + 1))
done

# The URL with the owner token in its fragment. The token never goes to the console.
ui_url_file="$logs/ui-url.txt"
printf '%s\n' "http://127.0.0.1:$port/#token=$owner" > "$ui_url_file"
chmod 600 "$ui_url_file"
echo "run folder: $run_dir"
echo "workers: $workers slot(s), agents$agents"
echo "UI: http://127.0.0.1:$port/  (open $ui_url_file for the URL with the owner token)"
echo "watch: sh $SCRIPT_DIR/watch.sh -d \"$run_dir\" -p $port -s $settle"
echo "stop: sh $SCRIPT_DIR/stop.sh -d \"$run_dir\""

if [ "$watch_take" = 1 ]; then
  # The stack keeps running until stop.sh runs. Its processes write to log files, not to this terminal.
  sh "$SCRIPT_DIR/watch.sh" -d "$run_dir" -p "$port" -s "$settle"
fi
