# Acan

Acan is a scope-first behavior gate for coding agents. The developer approves what an agent may change, a scope with obligations, before any code exists. Acan then runs the old app twice and the new app once, side by side, on each probe. The old runs start from the protected head, and the new run uses the exact merged commit that would land. Differences outside the scope bounce back to the agent with a reproducer. The developer rules only on what is left. Each ruling is recorded, and every later receipt must report it or label it not enforced.

The plant view shows where each agent went, what bounced, and any clash held before either worker writes. The developer decides on the anchored card.

Work in progress for the Meta Global AI Developer Hackathon.

## Why Meta models matter here

Muse Spark is the foreman: it splits each order into tasks and drafts the scope contract a human then confirms. Muse Spark Open is every worker: it reads back the task, claims it and writes the code. Acan checks the result itself, so no model's word decides whether a change lands.

- The routing in `crates/acan-llm/llm.example.toml` sends the foreman to Muse Spark and the workers to Muse Spark Open.
- Foreman: Muse Spark drafted a scope from a real order, and Acan's decoder accepted it. See `receipts/m01-scope-draft.md`.
- Workers: Muse Spark Open answered live through Baseten and Fireworks. See `receipts/p00-provider-smoke.md`.

## Status

What main proves today:

- The scope-first gate. Its contract is in `crates/acan-intent`. A worker's readback of its task is refused until a human confirms the scope.
- Live with receipts: a Muse Spark scope draft (`receipts/m01-scope-draft.md`) and six provider calls on three hosts (`receipts/p00-provider-smoke.md`).
- The vision witness, live on two hand-made candidates of the demo's dashboard page (`receipts/vision01-live-vision.md`). The vision witness is advisory. Its verdicts never set pass or fail.
- Muse Wonder, the advisory second look, live on one hand-made candidate (`receipts/vision02-live-wonder.md`). The first look called the candidate inconsistent, so Wonder ran once and also called it inconsistent. Its verdicts never set pass or fail.

In progress, with no numbers yet:

- T01, the tour director, is on main. Its two live calls, each on a landed receipt from the same test setup, gave one admitted tour (`receipts/director-live-2.md`). No other receipts have been run, and no measured comparison with the plain card has run.
- DF1, `acan receipt`, which runs the gate's contrast on any clone that has an app contract.
- The alpha release: a local app that takes a key for each provider the tester uses, an update check, a bug report, and a GitHub Pages replay.
- X01, the 12-order replay. No numbers are published before it finishes.

## Meta models

Every model role in the code is listed here. A role counts as live only when a row in `receipts/` shows a real call. The routes come from `crates/acan-llm/llm.example.toml`.

| Role | Model | Host and route | Receipts | Status |
| --- | --- | --- | --- | --- |
| Foreman: splits orders and drafts scopes | Muse Spark, `muse-spark-1.3` | Meta Model API (`api.meta.ai`), `[routing.foreman]` | p00 rows 1-2; m01 row 1 | Live, pass |
| Workers | Muse Spark Open, `meta-models/muse-spark-open-1026` | Baseten (`inference.baseten.co`), `[routing.worker]` | p00 rows 3-4 | Live, pass |
| Workers, second host | Muse Spark Open, `accounts/fireworks/models/muse-spark-open` | Fireworks (`api.fireworks.ai`) | p00 rows 5-6 | Live, pass. Fireworks serves Muse Spark Open, and the API echoed that model id. |
| Fast: clash explanations | Muse Spark Open | Baseten, `[routing.fast]` | none | Routed, not yet in a receipt |
| Vision witness, advisory | Muse Spark, `muse-spark-1.3`, with images | Meta Model API, `[routing.vision]` | vision01 rows 1-2 | Live, pass on two hand-made candidates |
| Director: proposes the receipt tour (T01) | Muse Spark, `muse-spark-1.3`, structured output | Meta Model API, `[routing.director]` or the foreman's route if unset | director-live-2 rows 1-2 | Live. Two calls, each on its own landed receipt. The second was admitted; the first was not, and its reason was not logged. The other roles in that run were stubbed. |
| Second look at vision (Muse Wonder), advisory | Muse Wonder, `muse-wonder-1.0` | Meta Model API, `[routing.wonder]`, called only after an inconsistent or unclear first look | vision02 row 3 | Live, one call on one hand-made candidate. Its verdict matched the first look's. Not yet run on an unclear first look. |

## Architecture

Acan is a Rust workspace. Each crate under `crates/` has one job.

| Crate | Role |
| --- | --- |
| `acan-core` | Acan's world model: orders, claims, clashes and gates as a pure event-sourced fold. |
| `acan-intent` | The contract of Acan's scope-first behavior gate: scopes, probes, observations, rulings, receipts and the plant view. |
| `acan-git` | Per-task git worktrees and queue-ordered landing onto the protected branch. |
| `acan-scan` | Scans a source repository with tree-sitter into a plant: machines, belts, and per-file symbols and imports. |
| `acan-llm` | Acan's single OpenAI-compatible model client. |
| `acan-agent` | The worker harness: a model-driven worker that takes a task from the origin over MCP. Builds the `acan-worker` binary. |
| `acan-witness` | Runs a witness check against a task's worktree and reports the outcome. |
| `acan-server` | The `acan` binary: one world's origin served over HTTP and WebSocket, with its event log in SQLite. |
| `acan-render` | The plant renderer: 3D meshes drawn with wgpu. |
| `acan-render-web` | Browser host for `acan-render`, built to wasm. |
| `acan-iso` | Isometric machine models as draw lists, ported from an MIT anatomy kit. Its license and source notes are in the crate (`LICENSE-anatomy`, `SOURCE`). |

The demo app that the live take runs against is a separate public repository: https://github.com/NDilanka/acan-demo.

## Acan and the Super-agent design

In the Super-agent design, a single writer keeps the sequence, a foreman splits the work, workers each build one task, and a person decides. Acan maps onto its parts as follows.

| Super-agent part | In Acan | Status |
| --- | --- | --- |
| Router | The single writer (`crates/acan-server/src/writer.rs`) owns the origin. The foreman's splits and scope drafts turn orders into tasks. | On main |
| Workers | Each `acan-worker` takes one task over MCP, builds it and reports. Muse Spark Open runs them. | On main |
| Threads | An order's work shows as trails: each drone leaves a trail of its recent moves on the plant map. | On main |
| Watchdog | A drone that holds a task and goes quiet turns stale after 2 minutes and away after 10. Drift marks (stuck, or a candidate out of scope) are planned. | Liveness on main, drift planned |
| Needs you (short channel) | The inbox lists each open question, clash, seal and residue that waits on you. The 100-word card cap is planned. | Inbox on main, cap planned |

The tour director orders a receipt's tour over the plant view. It routes no work, so it is not the Router.

## Model checks

The `tla/` folder holds two TLA+ models of the lane rules that the scheduler enforces, and TLC checks them. The properties, constants, results and open items are in `tla/README.md`.

## Run steps

### Prerequisites

- For the live take on Windows: Windows 10 or 11 with PowerShell 7, Git, Bun 1.4 or newer, and the Rust toolchain in `rust-toolchain.toml`.
- For the live take on Linux: a POSIX shell with Git, `jq`, `curl`, `nohup`, `ps`, `pgrep`, Bun 1.4 or newer, and the same Rust toolchain. See `demo/README.md`.
- For the UI bundle: the `wasm32-unknown-unknown` target and the `wasm-bindgen` CLI.
- For `cargo test --workspace`: Docker, and a clone of the demo repo with `ACAN_DEMO_DIR` set to its path. The witness tests read that checkout.
- For a live take: provider keys (see step 2 below).
- For the replay: PowerShell 7 on Windows or Linux, or a POSIX shell with `sha256sum` or `shasum`. No keys are needed. macOS is not tested.

### Replay without keys

```
pwsh demo/replay.ps1
```

This checks each receipt in `receipts/` against `receipts/SHA256SUMS`, then prints it. It reads no keys and makes no network calls. It has run on Windows and on Linux (PowerShell 7.4 in a container). macOS has not been tested.

`sh demo/replay.sh` does the same check with a POSIX shell and prints the same lines. `sh demo/replay.sh -v` checks the checksums only.

`replay/` holds one recorded run from 1 November 2026, exported with `acan export-replay`. The Pages site plays it at `?replay=1` with no keys. Its log has two scope drafts, three candidate receipts, one landed change and one failed land attempt. It has no clashes, so the clash and trail beats are not in it, and its log records no director call.

### Live take (needs your keys)

1. Clone the repo: `git clone https://github.com/NDilanka/acan.git`, then `cd acan`.
2. Copy `crates/acan-llm/.env.example` to `crates/acan-llm/.env` and fill in `META_API_KEY`, `BASETEN_API_KEY` and `FIREWORKS_API_KEY`. Copy `crates/acan-llm/llm.example.toml` to `crates/acan-llm/llm.toml`. Both copies are git-ignored.
3. Build the UI bundle once: `crates/acan-render-web/build.ps1`.
4. Start the take: `pwsh demo/run.ps1 -Watch -Settle 3`. On Linux, `sh demo/run.sh -w -S 3`.
5. Stop it: `pwsh demo/stop.ps1`. On Linux, `sh demo/stop.sh`.

The UI and API use port 7311, and the demo app uses port 7312. Both are set with parameters, and the environment variables `ACAN_RUN_ROOT` and `ACAN_DEMO_REPO` change the run folder and the demo repo. The full list is in `demo/README.md`.

`run.ps1` checks the tools and the keys before it starts anything. Then it waits for the codebase to map before it posts the order. `-CheckOnly` runs the checks alone, and `-Offline` runs a take with no keys and no model calls. `demo/README.md` has the details.

### Map the codebase on first view

By default, `acan serve` starts the codebase scan when the server starts. Pass `--map-on-first-view` to wait instead: the scan starts when the first authenticated page connects. Until then the project shows no machines and takes no writes. The first page then sees the map fill in from blank.

```
acan serve --repo <path> --map-on-first-view
```

The default is unchanged without the flag. The demo take does not pass it, since `run.ps1` waits for the map itself.

## X01: 12-order replay

In progress. No numbers are published before the run finishes.

## Honest limits

- The live take runs on Windows with PowerShell 7, and on Linux with the POSIX scripts in `demo/`. The POSIX path has run a take without provider keys. A live take on the POSIX path has not run yet. The take scripts under `demo/intent` are Windows-only.
- Live takes need your own provider keys. `llm.toml` and `.env` are git-ignored and are not in the repository.
- The take and the worker slots pass each worker its per-take local acan agent token in the `ACAN_AGENT_TOKEN` variable, not on the command line, so the process list does not show it. The control identity's token goes the same way. These tokens are created by the take for its own server. They are not provider keys.
- The probe driver runs in the same container as the candidate code. The container has a read-only root and all Linux capabilities dropped.
- `model_cost_micros` is stored in each order's repair budget, but no check reads it. The model cost cap is not enforced.
- The hosted replay is one recorded run without clashes (see Replay without keys). The recorded receipts replay the provider calls, not the full UI.

## Rulings disclosure

Rulings for orders o-2 to o-12 were AI-delegated. An AI coordinator made them under the author's delegation, not a human reviewer.

## Built during the hackathon (Oct 26 to Nov 4, 2026)

Everything in this repository was written during the hackathon, starting from the first commit on Oct 26, 2026.

Before the hackathon I did design research and made UI mockups. They live in [NDilanka/acan-research](https://github.com/NDilanka/acan-research). That repo is pre-existing research and is not part of this submission. No code from it is used here.

## License

MIT. See `LICENSE`. The vendored anatomy kit in `crates/acan-iso` keeps its own license file, `LICENSE-anatomy`.

## Contributors

NDilanka (solo).
