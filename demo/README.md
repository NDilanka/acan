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

`demo/intent/take.ps1` runs one take of the intent flow on a clean clone of this repository at the pin in the script. Each run gets its own folder under `C:\work\acan-takes`. Git runs only inside the clone, and the script checks the clone's top level before each git call. Keys come from `crates/acan-llm/.env` and reach the server through its environment only. Tokens go in the clone's ignored `.acan/tokens.json`. Every human step goes through the member HTTP API and appears in `summary.json`.

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

## Replay (X01, demo/intent/replay.ps1)

The replay runs the 12 orders in `demo/intent/orders.json` once each, and the four controls in `demo/intent/mutants.json`, through the real server with live models. Every outcome goes into the results, failures included. Nothing is curated, and no expected value is changed to finish a run.

`pwsh demo/intent/replay.ps1 -DryRun` reads only `orders.json` and `mutants.json` and prints the plan. It starts nothing and draws nothing. A live run is `pwsh demo/intent/replay.ps1 -Label <name>`. It needs Docker running, the built `acan.exe` and `acan-worker.exe` at `C:\cargo-targets\acan-x01\debug\`, and `crates/acan-llm/.env` and `llm.toml`. `-Only o-1` runs one order as a rehearsal; that run is reported partial. The default port is 7413. Ports 7411 and 7412 are refused.

The script refuses to start, before it writes anything, when any of these hold: the foreman is not Muse Spark on `meta`; the worker is not Muse Spark Open on `baseten`; either host does not answer on TCP 443; a key variable is missing from `.env` (names are checked, values are never read out); an input file is missing; the injector's anchor is not in the pinned `src/server.ts` exactly once; or `take.ps1` lacks the `-LibraryOnly` guard or any helper the replay calls.

The live path has not been run. `take.ps1` has the `-LibraryOnly` guard. Its main body and the replay both call five steps lifted into functions: `New-TakeClone`, `Start-TakeServer`, `Invoke-ScopeEdits`, `Invoke-ScopeConfirm` and `Invoke-ControlWithRuling`. The lifted steps were checked by parse and dot-source only; no live take or replay has run them since the lift. A live start stops in the preflight, before the run folder is written, when the guard or a helper is missing.

Decision inputs. A file that is present is checked before the draw (schema, order id, `decided_by`). A file that is missing pauses the run when its order needs it.

- `demo/intent/<id>-edits.json`, schema `acan-demo-intent-edits/2`, for each order that runs. To accept a draft as it is, use an edits file with empty `scope_edits`, `choice_rules` and `answers`.
- `demo/intent/rulings/<id>-rulings.json`, schema `acan-demo-intent-rulings/1`, needed only when a receipt has an out-of-scope item that no rule covers.

No file is needed per order for the injected change. The replay generates it (see Injected change).

### Who decides

Every scope and ruling decision for o-1 to o-12 is delegated by the owner to the coordinator, which is an AI. They are not human decisions, and the replay never records them as human. No order is human by default: the o-1 files name the coordinator too. Each decision file names its decider in `decided_by`: `coordinator, delegated by the owner`, which the record carries as `coordinator (delegated by the owner)`. A file that names `human (owner)` is labelled that way. A file with no `decided_by`, or any other value, stops the run.

The replay decides nothing itself. It sends the decisions the coordinator wrote, using the owner's member token. The files it writes are controls, labelled as controls and not as decisions: the m-broad-scope edits and the injected bundles.

### Pauses and resume

An order that needs a decision the replay does not have pauses the run. For a missing scope decision, the replay writes the foreman's draft to `<run folder>\<id>\scope-draft.json` and prints `needs_scope_decision <id> <draft path>`. For an out-of-scope difference that no rule covers, it writes the ruling draft for that episode to `<run folder>\<id>\ruling-draft-<episode>.json` and prints `needs_ruling <id> <episode> <draft path>`. It then stops the server, writes `summary.json` with status `paused`, and exits with code 3.

The coordinator writes `demo/intent/<id>-edits.json` or `demo/intent/rulings/<id>-rulings.json`, with `decided_by`, and runs `pwsh demo/intent/replay.ps1 -ResumeRun <run folder>`. The resume reuses the run's clone, tokens and draw (their hashes are checked) and continues the paused order from its `state.json` checkpoint. It does not post, split, draft or run a worker again. If the foreman's draft has changed since the pause, the resume stops with `draft_changed`, because the decision was written for the old draft.

### Injected change

Each drawn order gets one out-of-scope change: the response header `x-replay-injected: <order id>` on `GET /health`, a route that no order's scope names. The replay generates the bundle (`acan-demo-control/1`) from the pinned `src/server.ts`. Its anchor is the whole health handler, which must occur exactly once in the pin; the preflight checks that. The bundle goes to `<run folder>\controls\injected-<id>.json`, and its SHA-256 goes into `draw.json` before any candidate exists.

`control.ps1 -OnWorkerCandidate` applies the bundle to the files of the task worktree at its HEAD, and commits the injected header on top of that HEAD. The injected candidate runs after its order, on the order's control task: a split task that worker-r did not build (`Get-ControlTask`). That HEAD is the order's base, not worker-r's candidate, because worker-r's own task is the one the order's receipt is read from and it is never claimed by the injection. The worktree must be clean; the control refuses otherwise. The result records `base_source` and `base_head`, and the seeded line copies them. The control claims with `-RedeclareOnce`. If the control task is no longer open for a claim, the claim is refused and the line is `error`, not a catch. The gate has no ruling for the header, so an unruled injected item blocks the candidate. A catch therefore shows that the gate blocks that header on the control task's base. An order with one split task has no control task, so its seeded line is `not_run`. Live behaviour of this placement has not been run.

The draw happens after the preflight and before the clone. The seed comes from the OS random source at live start. The drawn orders are the four with the smallest SHA-256 of `<seed>|<order id>`, in `orders.json` order. The replay then writes one injected bundle for each drawn order. `draw.json` in the run folder holds the seed, the pool, the drawn list, the injector spec, the SHA-256 of each drawn order's bundle, and the hashes of `orders.json` and `mutants.json`. Its SHA-256 goes into `summary.json` (status `drawn`) before any clone or candidate exists. The seed and the drawn list are copied into `summary.json` only at the end of the run. A dry run never shows the draw.

Run folder: `<RunsRoot>\<label>-<stamp>\`, default `C:\work\acan-demo-runs\intent`. It holds `results.jsonl`, `summary.json`, `draw.json`, `logs\`, `controls\` (the injected bundles, the injected results and the m-missing-fixture copy) and one folder per order (its `state.json` checkpoint, the scope draft, ruling drafts and receipts).

Each line of `results.jsonl` is one of: an order (`kind: order`), the injected candidate of a drawn order (`kind: seeded`), or a control (`kind: control`). An order line records the actor, provider, `scope` (`drafted`, `human_edits` or `delegated_edits`), `scope_decided_by`, `rulings_decided_by`, `outcome`, `receipt_hash`, `merged_sha`, `wall_seconds`, `model_calls` (the `model_used` events of that order), `resumed_after_pause`, `errors`, and three catch flags. Order outcome values are `landed`, `blocked_out_of_scope`, `blocked_obligation_failed`, `blocked_violates_ruling`, `blocked_other`, `incomplete`, `setup_failed`, `draft_not_ready`, `draft_changed`, `needs_human_scope` (raised by a take step that needs a person), `ruling_refused`, `worker_timeout` and `worker_no_build`. Seeded lines use `caught`, `landed_not_caught`, `not_caught`, `not_run` or `error`. Control lines use `as_expected`, `not_as_expected`, `not_run` or `error`, with the expected outcome from `mutants.json` beside them. An order that pauses has no line until it finishes.

`summary.json` holds the three catch counts, computed separately from the flags on each line:

- `natural_ruling`: an order's receipt has a `violates_ruling` item whose ruling was recorded for an earlier order in the same run.
- `seeded_demonstrations`: a drawn order's seeded candidate did not land and has a blocking item (`out_of_scope`, `violates_ruling` or `obligation_failed`). The replay does not rule on seeded candidates, so an unruled out-of-scope item blocks them.
- `base_only`: a candidate did not land, has `obligation_failed` items, and has no `violates_ruling` item.

One line can count in more than one of the three.

Supersedes: `o-9` supersedes `o-2`, and `o-12` supersedes `o-1`. `HumanCmd::PostOrder` has no supersedes field (`crates/acan-core/src/event.rs:75-82`), so each is posted as an independent order. The line for each carries `supersedes_handling`: the target's outcome in the run, and any event whose type contains "supersed". None is expected.

Attempts: one per order in a run. The replay never posts an order twice. A pause is not a failed attempt: the order resumes from its checkpoint with `-ResumeRun`. The product's own bounce and repair loop runs inside the attempt. A run that stopped on a failure is reported `incomplete`; it cannot be resumed, so a new start is a new run folder.

Tasks of one order. The origin's `list_tasks` has no order filter, and a worker takes the first open task on the board. So worker-r's receipt is read from the task its own events name (`Get-WorkerTask`: its last `build_reported`, else its last `lease_granted`), and an order's receipt waits on that task only. The other split tasks of the order are not built in the run. After the order and its controls, `Close-ReplayOrder` ends the order's tasks that did not land, through `cancel_order` on the member route, so no later order's worker claims them. Each order's `state.json` records what was dropped, and `summary.json` lists it under `leftovers`. A resumed run does not send `cancel_order` twice (`Close-OrderLeftovers` checks for `order_cancelled`).

Controls. `m-overfit` and `m-harmless-difference` run on the control task of `o-1` (a split task worker-r did not build) through `control.ps1`; take.ps1 runs them on its first task. Each waits for its task to be open for the claim (`-ClaimWaitSec`, default 600), and `m-overfit` runs with `-RedeclareOnce`. If `m-overfit` is not blocked with an obligation_failed item, or ends in an error, `m-harmless-difference` is not run: its line is `not_run` with the reason, as take.ps1 stops o-1 there (take.ps1:850). The run then goes on to the next order. Otherwise `m-harmless-difference` is out of scope at first. The `m-overfit` receipt is ruled first (D02Q), then the `m-harmless-difference` receipt, from `rulings\o-1-rulings.json`; then the re-gate and the land. Its `ruling_decided_by` is the rules file's `decided_by`, which is `coordinator, delegated by the owner` for o-1.

`m-broad-scope` is a separate order, `control-m-broad-scope`, with o-1's sentence. Its draft is revised with broad items: one copy with method `ANY` of each link-API may item the draft names (`/api/links` and `/api/links/:code`). `mutants.json` says `/api/*`, which the route template grammar refuses (`crates/acan-intent/src/types.rs:332`), so the control uses the broadest templates the grammar allows. Confirmation is tried with no broad acknowledgement. It is `as_expected` only when the refusal says the broad item `needs its own acknowledgement` (`crates/acan-core/src/lane.rs:461`), no `scope_confirmed` event exists, and no claim, build or land event exists on its tasks. Its edits file is labelled `control (m-broad-scope), not a decision`. No worker starts for it. It has not been run.

`m-missing-fixture` writes a copy of the pinned f-accounts recipe without step `f5`, with its SHA-256 and a static prediction of the steps that would be missing. The gate is not run on the copy, so the control is `not_run`, never a pass, and its results line carries that reason. The gate reads the probe corpus only from the protected-head snapshot: `crates/acan-server/src/observe.rs:54` names the file (`probes/core.json`), `observe.rs:284-296` writes the snapshot from the protected head, and `observe.rs:305-308` reads it. A copy in a run repository is not read, so this control cannot be gated without a product change. For the same reason a run cannot reach `complete`.

The expected outcome of all four controls is taken from `mutants.json`. None is changed here.

`summary.json` has `status: complete` only when all 12 orders have a verdict or a recorded failure, all four controls have a result that is not `not_run` or `error`, and the run was not a rehearsal. Otherwise `incomplete_reasons` lists each reason. A paused run has `status: paused` and its pause in `pause`.

The `decisions` block counts scope and ruling decisions by decider: `scope_human`, `scope_delegated`, `rulings_human` and `rulings_delegated`. Every count is per order that made the decision, and the coordinator's decisions are never counted as human.

Exit codes: 0 when the run finished (complete or incomplete), 1 when it stopped on a failure, 2 when the secret scan found a token or key in a log or result, and 3 when it paused for a decision.
