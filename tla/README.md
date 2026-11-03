# tla

TLA+ models for the statechart work. The StoppedLane and LaneBudget models are below.

## StoppedLane

Constants: `Tasks = {a, b}`. `Fixed` is FALSE or TRUE. `Mutant` is `"none"`, `"keep_lease"` or `"unstop"`.

Configs (each checks TypeOK, NoBuildWhenStopped, LeaseMeansClaimed as invariants, and LeasesEnd, StopIsFinal as properties):

| cfg | Fixed | Mutant | states (all reachable) |
| --- | --- | --- | --- |
| `StoppedLane_today.cfg` | FALSE | none | 46 |
| `StoppedLane_fixed.cfg` | TRUE | none | 28 |
| `StoppedLane_keep_lease.cfg` | TRUE | keep_lease | 33 |
| `StoppedLane_unstop.cfg` | TRUE | unstop | 28 |

The state count is plain TLC with no rules on each cfg. Every cfg is under the 500-state chart budget.

Init uses `stopped = FALSE`. The v1 text used `~stopped`, which does not constrain the variable, and TLC rejects it.

## Commands

```sh
pwsh tla/run.ps1
statechart tla/StoppedLane.tla --config tla/StoppedLane_today.cfg --view tla/views/stopped_lane.view.ts --out tla/out/StoppedLane_today
statechart tla/LaneBudget.tla --config tla/LaneBudget_today_cap1.cfg --view tla/views/lane_budget.view.ts --out tla/out/LaneBudget_today_cap1
```

`run.ps1` writes `tla/out/<cfg>/` and copies `tla2tools.jar` into `tla/.jar/`. Both are git-ignored. It exits 0 only when every expectation in both models holds and the statechart command and plain TLC agree on every rule. The statechart command needs `mise` on PATH; `run.ps1` puts `~/.local/bin` first.

## Results

Expected and recorded by `run.ps1`:

- `_today` breaks NoBuildWhenStopped (5 steps: Claim a, Clear a, Built a, Stop a, Clear a) and LeasesEnd (5 steps). TypeOK, LeaseMeansClaimed and StopIsFinal hold.
- `_fixed` holds all five rules.
- `_keep_lease` breaks LeaseMeansClaimed (4 steps). LeasesEnd also breaks (4 steps). This is the expected result, and it corrects the plan, which expected only LeaseMeansClaimed to break. The lease that survives a stop sits on an open task, and `Release` has no fairness on purpose, so nothing forces the agent to let it go. LeasesEnd is the rule that catches that.
- `_unstop` breaks StopIsFinal (5 steps). The other four hold.

## LaneBudget

Constants: `Items = {a, b}`, `RepairRounds = 2`, `MaxEpisodes` is 0, 1 or 3, `Fixed` is FALSE or TRUE, and `Mutant` is `"none"`, `"off_by_one"` or `"unstop"`.

The cap is per round. `classify` in `crates/acan-core/src/repair.rs` (the block that begins with the comment "`max_episodes` caps the open, non-fixed episodes one round considers") gives each round the first `max_episodes` bounceable episodes. The rest stay open for a later round. The lane stops for good only when bounceable work is left and either the rounds are used up or the batch is empty, which happens only when `max_episodes` is 0. The model follows that rule. A receipt bounces a batch K of size `min(MaxEpisodes, bounceable)`, and the lane stops when `rounds >= RepairRounds` or K is empty. The model allows any batch of that size. The code takes the fewest bounces first, then by identity, so the model covers more batches than the code takes.

Configs (each has `CHECK_DEADLOCK FALSE`, invariants `RoundsBounded` and `StopAfterRound`, and properties `ResidueClears` and `StopIsFinal`):

| cfg | MaxEpisodes | Fixed | Mutant | states (all reachable) |
| --- | --- | --- | --- | --- |
| `LaneBudget_today_cap1.cfg` | 1 | FALSE | none | 67 |
| `LaneBudget_today_cap3.cfg` | 3 | FALSE | none | 64 |
| `LaneBudget_fixed_cap3.cfg` | 3 | TRUE | none | 132 |
| `LaneBudget_off_by_one.cfg` | 3 | TRUE | off_by_one | 180 |
| `LaneBudget_unstop.cfg` | 3 | TRUE | unstop | 156 |
| `LaneBudget_cap0.cfg` | 0 | FALSE | none | 16 |

The state count is plain TLC with no rules on each cfg. Every cfg is under the 500-state chart budget.

### Results

Recorded by `run.ps1`. The paths are the report's: "shortest" where the skill reports a shortest path, and TLC's counterexample for a property.

- `_today_cap1` breaks ResidueClears. RoundsBounded, StopAfterRound and StopIsFinal hold. TLC's counterexample has 5 steps. The shortest route to the bug has 2 steps: receipt leaves residue on a, then owner rules on a. After that, passing receipts repeat forever and a stays residue.
- `_today_cap3` breaks ResidueClears in the same way, with the same 5-step counterexample. The other three rules hold.
- `_fixed_cap3` holds all four rules.
- `_off_by_one` breaks RoundsBounded in 3 steps: three bounce receipts reach round 3 of 2. The other three rules hold.
- `_unstop` breaks StopIsFinal in 4 steps: two bounce receipts, a stop (budget runs out), then the owner unstops the lane. The other three rules hold.
- `_cap0` breaks StopAfterRound in 1 step: a receipt bounces an item with `MaxEpisodes = 0`, and the lane stops at round 0. This is the only way to stop at round 0 under the current code. ResidueClears breaks in 3 steps. `run.ps1` records that result without gating on it, because this cfg's expectation names only StopAfterRound. RoundsBounded and StopIsFinal hold.

The chart for `_today_cap1` is `tla/out/LaneBudget_today_cap1/chart.mmd`. It draws 31 states and 83 arrows. Every budget-runs-out arrow starts at "2 of 2 rounds used", and no bounce arrow does. The labels count items rather than naming them, because the items are interchangeable.
