# tla

TLA+ models for the statechart work. This stub covers S1 (StoppedLane). R1 adds the rest.

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
```

`run.ps1` writes `tla/out/<cfg>/` and copies `tla2tools.jar` into `tla/.jar/`. Both are git-ignored.

## Results

Expected and recorded by `run.ps1`:

- `_today` breaks NoBuildWhenStopped (5 steps: Claim a, Clear a, Built a, Stop a, Clear a) and LeasesEnd (5 steps). TypeOK, LeaseMeansClaimed and StopIsFinal hold.
- `_fixed` holds all five rules.
- `_keep_lease` breaks LeaseMeansClaimed (4 steps). LeasesEnd also breaks (4 steps). This is the expected result, and it corrects the plan, which expected only LeaseMeansClaimed to break. The lease that survives a stop sits on an open task, and `Release` has no fairness on purpose, so nothing forces the agent to let it go. LeasesEnd is the rule that catches that.
- `_unstop` breaks StopIsFinal (5 steps). The other four hold.
