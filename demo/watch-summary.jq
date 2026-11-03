# Builds the summary of one take from its events. Input: the events of the take, slurped into one array in
# seq order. Arguments: $writes (the worker writes, each with its UTC time, path and task), $usage (the usage
# report, or null), $live_failures (a count) and $watch_end (why the watch stopped). Field order and the
# meaning of each field follow watch.ps1, so the two summaries compare equal.

include "watch-lib";

def events($type): [.[] | select(.event.type == $type)];
def first_at($type): events($type) | .[0].at;
def el($origin; $ms): if $ms == null then null else elapsed_from($origin; $ms) end;

([.[] | select(.event.type == "order_posted") | .at] | .[0]) as $origin
| (reduce events("landed")[] as $l ({}; .[($l.event.task | tostring)] = $l.at)) as $landed
| ($landed | length) as $landed_count
| (reduce events("declared")[] as $d ({};
    ("t-\($d.event.decl.task)") as $k
    | if has($k) then . else .[$k] = $d.at end)) as $declared_by
| (events("order_split") | last | (.event.tasks // [] | length)) as $task_count
| ([events("clash_found")[]
    | {clash: .event.clash, kind: .event.kind, seq: .seq, atMs: .at, file: .event.key.at.file,
       a: "t-\(.event.key.a)", b: "t-\(.event.key.b)"}]) as $clashes
| ($clashes | .[0]) as $c0
| (if $c0 == null then null else ($c0.atMs | iso7_from_ms) end) as $clash_utc
| ($writes | map(.utc) | min) as $first_write_utc
| (if $c0 == null then [] else [$writes[] | select(.path == $c0.file)] end) as $contested
| ($contested | map(.utc) | min) as $first_contested_utc
| (if $clash_utc == null then false
   else ($first_contested_utc == null or $clash_utc < $first_contested_utc) end) as $clash_before_write
| (if $c0 == null then null
   else
     ($declared_by[$c0.a]) as $da
     | ($declared_by[$c0.b]) as $db
     | (if $da != null and $db != null and $db > $da then $c0.b else $c0.a end)
   end) as $blocked_task
| (if $c0 == null then null
   else ([$contested[] | select(.task == $blocked_task)] | map(.utc) | min) end) as $blocked_write_utc
| (if $c0 == null then null
   else ($blocked_write_utc == null or $clash_utc < $blocked_write_utc) end) as $clash_before_blocked_write
| (($task_count > 0) and ($landed_count == $task_count)) as $all_landed
| (if $c0 == null then (if $all_landed then "no_clash_landed" else "no_clash_incomplete" end)
   elif ($clash_before_write | not) then "clash_after_write"
   elif $all_landed then "clash_before_write_landed"
   else "clash_before_write_not_landed"
   end) as $outcome
| (if $first_write_utc == null then null else ($first_write_utc | ms_from_iso7) end) as $first_write_ms
| (if $first_contested_utc == null then null else ($first_contested_utc | ms_from_iso7) end) as $first_contested_ms
| (reduce events("model_used")[] as $m ({}; .[$m.event.step] = (1 + (.[$m.event.step] // 0)))) as $model_steps
| {
    outcome: $outcome,
    watch_end: $watch_end,
    tasks: $task_count,
    landed: $landed_count,
    clashes: $clashes,
    clash_before_write: $clash_before_write,
    clash_utc: $clash_utc,
    first_write_utc: $first_write_utc,
    writes: ($writes | length),
    counts: {
      readbacks: (events("readback_posted") | length),
      declares: (events("declared") | length),
      claims: (events("lease_granted") | length),
      clashes: ($clashes | length),
      landed: $landed_count,
      writes: ($writes | length)
    },
    seconds: {
      split: el($origin; first_at("order_split")),
      last_readback: el($origin; ([events("readback_posted")[] | .at] | max)),
      reviewed: el($origin; first_at("readbacks_reviewed")),
      first_claim: el($origin; ([events("lease_granted")[] | .at] | min)),
      first_declare: el($origin; ([events("declared")[] | .at] | min)),
      last_declare: el($origin; ([events("declared")[] | .at] | max)),
      clash_found: el($origin; first_at("clash_found")),
      first_contested_write: el($origin; $first_contested_ms),
      first_write: el($origin; $first_write_ms),
      settled: el($origin; first_at("clash_settled")),
      last_land: el($origin; ([$landed[]] | max))
    },
    model_steps: $model_steps,
    live_call_failures: $live_failures,
    usage: $usage,
    contested_file: (if $c0 == null then null else $c0.file end),
    blocked_task: $blocked_task,
    clash_before_blocked_write: $clash_before_blocked_write,
    blocked_first_write_utc: $blocked_write_utc,
    first_contested_write_utc: $first_contested_utc,
    declared_ms: $declared_by,
    origin_known: ($origin != null)
  }
