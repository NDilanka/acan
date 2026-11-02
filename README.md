# Acan

Acan replaces the chat window between a developer and their coding agents with a live map of the codebase. Every agent, every scope it may work in, every conflict and every piece of evidence sits in one shared place, so you can direct several agents at once and see trouble before code is written.

Acan is a scope-first contract gate for coding agents. An order becomes a scope contract that names the files a worker may change and what must stay the same, and the gate checks it before any code is written.

When two workers declare overlapping files, Acan pins the clash on the map and holds both before either one writes.

Work in progress for the Meta Global AI Developer Hackathon.

## Why Meta models matter here

Muse Spark is the foreman: it splits each order into tasks and drafts the scope contract a human then confirms. Muse Spark Open is every worker: it reads back the task, claims it and writes the code. Acan checks the result itself, so no model's word decides whether a change lands.

- The routing in `crates/acan-llm/llm.example.toml` sends the foreman to Muse Spark and the workers to Muse Spark Open.
- Foreman: Muse Spark drafted a scope from a real order, and Acan's decoder accepted it. See `receipts/m01-scope-draft.md`.
- Workers: Muse Spark Open answered live through Baseten and Fireworks. See `receipts/p00-provider-smoke.md`.

## Status

- Live with receipts: a Muse Spark scope draft (`receipts/m01-scope-draft.md`) and six provider calls on three hosts (`receipts/p00-provider-smoke.md`).
- Clash lane: proven live on Oct 28 (two tasks clashed on `src/middleware.ts`, were settled with one key before either wrote, then both landed). Since the scope-first gate landed, a take must confirm a scope before workers can claim, and the demo script is being updated for that.
- In progress: a live two-order take (foreman draft, human confirmation, gate, a recorded ruling, and a second order blocked by that ruling).

## Meta models

| Role | Model | Host | Receipts | Status |
| --- | --- | --- | --- | --- |
| Foreman | Muse Spark, `muse-spark-1.3` | Meta Model API (`api.meta.ai`) | p00 rows 1-2; m01 row 1 | Live, pass |
| Worker | Muse Spark Open, `meta-models/muse-spark-open-1026` | Baseten (`inference.baseten.co`) | p00 rows 3-4 | Live, pass |
| Worker, second host | Muse Spark Open, `accounts/fireworks/models/muse-spark-open` | Fireworks (`api.fireworks.ai`) | p00 rows 5-6 | Live, pass. Fireworks serves Muse Spark Open, and the API echoed that model id. |
| Vision witness (in progress) | Muse Spark, `muse-spark-1.3`, image input | Meta Model API | pending | Being built. Not yet proven live inside Acan. |

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

## Run steps

### Prerequisites

- For the live take: Windows 10 or 11 with PowerShell 7, Git, Bun 1.4 or newer, and the Rust toolchain in `rust-toolchain.toml`.
- For the UI bundle: the `wasm32-unknown-unknown` target and the `wasm-bindgen` CLI.
- For `cargo test --workspace`: Docker, and a clone of the demo repo with `ACAN_DEMO_DIR` set to its path. The witness tests read that checkout.
- For a live take: provider keys (see step 2 below).
- For the replay: PowerShell 7 on Windows or Linux. No keys are needed. macOS is not tested.

### Replay without keys

```
pwsh demo/replay.ps1
```

This checks each receipt in `receipts/` against `receipts/SHA256SUMS`, then prints it. It reads no keys and makes no network calls. It has run on Windows and on Linux (PowerShell 7.4 in a container). macOS has not been tested.

### Live take (needs your keys)

1. Clone the repo: `git clone https://github.com/NDilanka/acan.git`, then `cd acan`.
2. Copy `crates/acan-llm/.env.example` to `crates/acan-llm/.env` and fill in `META_API_KEY`, `BASETEN_API_KEY` and `FIREWORKS_API_KEY`. Copy `crates/acan-llm/llm.example.toml` to `crates/acan-llm/llm.toml`. Both copies are git-ignored.
3. Build the UI bundle once: `crates/acan-render-web/build.ps1`.
4. Start the take: `pwsh demo/run.ps1 -Watch -Settle 3`.
5. Stop it: `pwsh demo/stop.ps1`.

The UI and API use port 7311, and the demo app uses port 7312. Both are set with parameters, and the environment variables `ACAN_RUN_ROOT` and `ACAN_DEMO_REPO` change the run folder and the demo repo. The full list is in `demo/README.md`.

## X01: 12-order replay

Pending. This runs after the two-order take lands. No numbers are published before then.

## Honest limits

- The live take runs on Windows only. Its scripts use PowerShell 7 with Windows APIs.
- Live takes need your own provider keys. `llm.toml` and `.env` are git-ignored and are not in the repository.
- The take passes each worker a per-take local acan agent token on the command line. These tokens are created by the take for its own server. They are not provider keys.
- The probe driver runs in the same container as the candidate code. The container has a read-only root and all Linux capabilities dropped.
- `model_cost_micros` is stored in each order's repair budget, but no check reads it. The model cost cap is not enforced.
- There is no hosted replay yet. The recorded receipts replay the provider calls, not the full UI.

## Rulings disclosure

Rulings for orders o-2 to o-12 were AI-delegated. An AI coordinator made them under the author's delegation, not a human reviewer.

## Built during the hackathon (Oct 26 to Nov 4, 2026)

Everything in this repository was written during the hackathon, starting from the first commit on Oct 26, 2026.

Before the hackathon I did design research and made UI mockups. They live in [NDilanka/acan-research](https://github.com/NDilanka/acan-research). That repo is pre-existing research and is not part of this submission. No code from it is used here.

## License

MIT. See `LICENSE`. The vendored anatomy kit in `crates/acan-iso` keeps its own license file, `LICENSE-anatomy`.

## Contributors

NDilanka (solo).
