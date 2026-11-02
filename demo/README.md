# Demo take

This folder stages one live take of the Acan demo. Each take clones the demo repository into its own run folder, starts the Acan server with the UI, and posts one order. Three worker slots take the three tasks of the split. The split can produce tasks that all change `src/middleware.ts`. When a task declares a file that a sibling has also declared, the origin raises a clash and holds both before any build. A person settles the clash in the UI, or `watch.ps1` settles it. The run then continues through build, gate, and land.

## Run a take

On Windows, this section applies. Requires Windows 10 or 11 with PowerShell 7 (the scripts use `Get-NetTCPConnection` and kernel32), Git, Bun 1.4 or newer, and the Rust toolchain from `rust-toolchain.toml`. A live take also needs the `wasm32-unknown-unknown` target and the `wasm-bindgen` CLI, because the UI bundle is built in step 1. On Linux, see [Run a take on Linux or macOS](#run-a-take-on-linux-or-macos).

A live take needs provider keys. Copy `crates/acan-llm/.env.example` to `crates/acan-llm/.env` and fill in `META_API_KEY`, `BASETEN_API_KEY` and `FIREWORKS_API_KEY`. Copy `crates/acan-llm/llm.example.toml` to `crates/acan-llm/llm.toml`. Both copies are git-ignored. Without keys, the recorded artifacts in `receipts/` replay the take instead (see the top-level README).

1. Build the UI bundle once per worktree. Run `crates/acan-render-web/build.ps1`.
2. Start the take. Run `pwsh demo/run.ps1 -Watch -Settle 3` as one background job. It starts the server, the demo app, and the three worker slots, then follows the take in the same process until it ends. The process exits when the watch ends, and the stack keeps running until `stop.ps1`. Before it starts the stack, it clears the inherited output handles, so the background job ends with the watch and does not wait for the stack's pipes. It stops if a port is already listening. Settle 3 serializes each clash the way the menu's third row does. Use `-Settle 0` to settle in the UI instead.
3. Follow the take without `-Watch`. Run `pwsh demo/watch.ps1 -RunDir "<run folder>" -Settle 3` as its own background job. A watch started from a short shell dies when that shell returns, so keep it in a background job.
4. Open the UI. Open the URL in `<run folder>\logs\ui-url.txt`. The URL holds the owner token in its fragment, so do not share it.
5. Stop the take. Run `pwsh demo/stop.ps1`. It stops only the processes the take started. With no `-RunDir`, both scripts use the latest take.

Each take gets its own run folder, `<run root>\take-<yyyyMMdd-HHmmss>`. `<run root>\current.txt` names the latest one. `run.ps1` never reuses a folder, and it stops the previous take's processes without touching that folder. So an earlier take keeps its logs, and a stale process writes only into its own take.

## Run a take on Linux or macOS

The POSIX scripts run the same take, in the same steps, with the same run folders and logs. They are `run.sh`, `watch.sh`, `stop.sh`, `worker-slot.sh` and `replay.sh`. `lib.sh` holds their shared helpers. `watch-lib.jq`, `watch-event.jq` and `watch-summary.jq` do the event work for `watch.sh`. The scripts need only `sh` and the tools listed below.

Requires `sh`, Git, `jq`, `curl`, `nohup`, `ps` and `pgrep`, Bun 1.4 or newer, and the Rust toolchain from `rust-toolchain.toml`. The scripts were tested with jq 1.7 under dash on Ubuntu 24.04.

1. Build the UI bundle with the steps in `crates/acan-render-web/build.ps1`. The repository has no POSIX build script yet.
2. Copy the key and routing files as on Windows, or point the take at other copies with `ACAN_ENV_FILE` and `ACAN_LLM_FILE`.
3. Start the take with `sh demo/run.sh -w -S 3`. The `-w` flag follows the take in this process until it ends. Without it, `run.sh` returns once the stack is running. To follow the take from another shell, run `sh demo/watch.sh -s 3` in a background job, as on Windows.
4. Open the URL in `<run folder>/logs/ui-url.txt`. It holds the owner token, so do not share it.
5. Stop the take with `sh demo/stop.sh`. With no `-d`, it stops the latest take.

Replay the recorded receipts with `sh demo/replay.sh`. `-v` checks the checksums only.

The POSIX options are `-d` (run folder), `-R` (demo repo), `-p` (UI and API port), `-P` (demo app port), `-m` (machine), `-n` (worker slots), `-s` (sentence), `-t` (wait seconds), `-S` (settle, used with `-w`) and `-w` (follow the take). `watch.sh` takes `-d`, `-p`, `-s`, `-t` (timeout seconds), `-i` (poll seconds) and `-m` (refused polls before it stops). The environment variables are the ones above, plus `ACAN_LLM_FILE`, `ACAN_ENV_FILE` and `CARGO_TARGET_DIR`.

These differ from the PowerShell scripts.

- `stop.sh` sends SIGINT to `acan serve`, so it shuts down cleanly. It sends SIGTERM to the other processes and SIGKILL after 10 seconds if one is still running. `stop.ps1` uses `taskkill /F`.
- `watch.sh` polls once a second. `watch.ps1` polls every 500 ms. Without `-p`, `watch.sh` uses the port recorded in the run folder.
- A failed poll reports the HTTP status, or `no answer on port N`. The PowerShell watch prints the .NET error text.
- The take scripts under `demo/intent` (`take.ps1`, `ruling.ps1` and `control.ps1`) are Windows-only. They use Windows paths, `acan.exe` and Docker through Windows, and have no POSIX port.

These checks ran on Ubuntu 24.04 with dash. The POSIX replay printed the same output as `replay.ps1` on the recorded receipts, including a changed receipt and a missing one. The POSIX watch wrote the same summary, raw events and settle commands as `watch.ps1` on one recorded event log, with settle options 1 and 3, a refused token, and a server that was down. Only the wording of a transport error differs. A full take ran without provider keys: the stack started, the order posted, the watch ran, a second watch was refused, and `stop.sh` stopped every process. A live take with provider keys has not run on the POSIX path yet. macOS has not been tested.

## Ports, paths and overrides

| Setting | Default | Override |
| --- | --- | --- |
| UI and API (`acan serve`) | port 7311 | `-Port` (`-p` in the POSIX scripts) |
| Demo app (the URL shortener) | port 7312 | `-DemoPort` (`-P` in the POSIX scripts) |
| Run root (holds `take-*` folders) | `target\acan-run` in this checkout | `ACAN_RUN_ROOT` |
| Demo repo to clone | `https://github.com/NDilanka/acan-demo.git` | `ACAN_DEMO_REPO` or `-DemoRepo` (a local path works; `-R` in the POSIX scripts) |

Port 3000 is not the take's default, because many machines already use it. Before it clones or builds anything, `run.ps1` checks both ports. If one is taken, it stops and names the process that holds it and the parameter to change, for example `-DemoPort 7413`. The demo app still listens on 3000 when you run it by hand with `bun start`.

The machine of `src/middleware.ts` is `middleware`. The scanner gives each file in a small repo's `src` folder its own machine, named by its stem. `run.ps1` reads the plant from `/api/plant` and stops if the `-Machine` name is missing. The plant list is printed at the start of the run.

Each worker slot runs `acan-worker` again after every task, because one worker process builds one task and exits. `worker-slot.ps1` runs the loop, and each run writes its own log pair, such as `worker-1.2.err.log`. There is one slot per task, because a task nobody has claimed holds the order's declare barrier.

The worker plans against the repository, not from memory. Before it declares, it lists the tracked files and sends the full text of the files in its task's scope machines. A plan's paths must name a tracked file, or a new file in a directory the repository already has. A missing prefix or a unique basename resolves to its file, and the worker logs the fix (`plan fix on t-0: ...`). Any other path is refused with its reason. The worker sends the refusal back with the file list, up to three planning calls, and then skips the task with the reason. Each release of a claim is logged with its reason (`released t-0: ...`), and each refused write or commit is logged too.

## Watch rules

`watch.ps1` is one watch per take.

- It refuses to start when the take's `logs\events.jsonl` already exists. Logs are never truncated or overwritten.
- It writes `logs\watch.lock` with its pid and start time while it runs. A second watch on the same take refuses to start while the lock holds a live process.
- It stops on its own when the take has landed every task, when the 1500 second timeout passes, when the server refuses the owner token (HTTP 401 or 403), or after 10 polls in a row fail.

`summary.json` records which of these ended the watch in `watch_end`: `all_landed`, `timeout`, `token_refused`, or `server_gone`.

## Read the result

`demo/watch.ps1` writes three files under `<run folder>\logs`.

- `timeline.txt` lists each origin event with its time from the order post.
- `events.jsonl` holds the raw events from `GET /api/events`.
- `summary.json` holds the outcome, the watch end, the counts, the step times, and the usage totals.

The step times in `seconds` are measured from the order post. They are `split`, `last_readback`, `reviewed`, `first_claim`, `first_declare`, `last_declare`, `clash_found`, `first_contested_write`, `first_write`, `settled`, and `last_land`. The `counts` object holds readbacks, declares, claims, clashes, landed tasks, and writes.

The clash rules in `summary.json` are these.

- `clash_before_write`: the first clash was raised before any task wrote its contested file. This is the rule the demo needs.
- `clash_before_blocked_write`: the first clash was raised before the task it held back, the one that declared second, wrote the file.
- `contested_file`, `blocked_task`, and `declared_ms` name the file, the held task, and each task's declaration time in milliseconds.

The outcome is one of these values.

- `clash_before_write_landed`: the first clash was raised before any write to its file, and every task landed.
- `clash_before_write_not_landed`: the first clash was raised before any write to its file, and some task did not land.
- `clash_after_write`: a task wrote the contested file before the first clash was raised.
- `no_clash_landed` and `no_clash_incomplete`: the split produced no blocking overlap, or the take did not finish.

The write times come from the worker logs. Each write logs one line when the write returns. The clash times come from the origin.

## Why a clash can be missed

The origin holds every task of the order from building while a sibling is still open, meaning not yet declared. The hold lifts when every sibling has declared, or 90 seconds after the first declaration, whichever comes first. A clash therefore comes before any build when all three tasks declare within 90 seconds of the first one. If a task declares later than that, the hold lifts, the first task can write, and the late declaration is a clash after a write. The summary shows this as `clash_after_write`.

## Intent take (demo/intent)

`demo/intent/take.ps1` runs one take of the intent flow on a clean clone of this repository at the pin in the script. Each run gets its own folder under `D:\dev\acan-takes`. Git runs only inside the clone, and the script checks the clone's top level before each git call. Keys come from `crates/acan-llm/.env` and reach the server through its environment only. Tokens go in the clone's ignored `.acan/tokens.json`. Every human step goes through the member HTTP API and appears in `summary.json`.

Run it with `pwsh demo/intent/take.ps1 -Label <name>`. It needs Docker running, the built `acan.exe` at `C:\cargo-targets\acan-d02\debug\acan.exe`, and `crates/acan-llm/.env` and `llm.toml`. It has no offline mode. Each model call is live, and `summary.json` records the provider for each role.

The take has two phases, because the foreman's draft changes between runs. Phase one, `-DraftOnly`, posts the order, waits for the draft, writes it to `<run folder>\scope-draft.json`, and stops with outcome `draft_ready`. A person reads the draft and writes the edits for that draft. Phase two, `-ResumeRun <run folder>`, reuses that run's clone, database and draft, applies the edits, confirms the scope, and checks the agent refusals.

Human edits live in `demo/intent/o-1-edits.json` (schema `acan-demo-intent-edits/2`). Each edit is logged as a human step in `summary.json`. Its ops are `remove_may`, `replace_must_keep`, `add_must_keep` and `add_may`, and each names a draft item by id. An op whose id is not in the draft stops the take. `choice_rules` match an open choice by its question text and either answer it with the option that matches `option_label_match` or remove it. A choice that matches a rule with no matching option and `when_no_option_matches: fail` stops the take. A choice that matches no rule must be answered in `answers`, or the take stops. Every change goes into one `revise_scope` through the owner's member route.

`summary.json` holds each step's actor, provider, result and evidence, and a `checks` object for the seven checks. A check that is not built reports `not_run` with its reason.

## Layout

- `lib.ps1`: shared helpers for the scripts, including the current-take pointer and the process check.
- `run.ps1`: clones the demo into a new take folder, writes the tokens, starts the processes, registers the agents, and posts the order.
- `worker-slot.ps1`: runs `acan-worker` for one agent again after each task.
- `watch.ps1`: follows the event log, settles clashes when asked, and writes the result.
- `stop.ps1`: stops the processes recorded in `<run folder>\logs\run.json`.
- `run.sh`, `watch.sh`, `stop.sh`, `worker-slot.sh` and `replay.sh`: the POSIX scripts for the same steps. `lib.sh` holds their shared helpers.
- `watch-lib.jq`, `watch-event.jq` and `watch-summary.jq`: the timeline lines, settle choices and summary that `watch.sh` builds with jq.

## Tokens

`run.ps1` creates one owner token and one agent token per worker, three in all, and writes them to `<run folder>\.acan\tokens.json`. The server reads that file at start, so it mints no token of its own. The owner token is never printed to the console.
