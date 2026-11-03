# Input: the events of one poll, in seq order. Arguments: $origin (ms when the order was posted, or null
# before that) and $settle (0 when a human settles in the UI, 1 to 3 for the menu row this watch takes).
# Output: one JSON record per event, {line, settle}. line is the timeline text, as watch.ps1 writes it.
# settle is null, or the clash and the choice this watch sends for it.

include "watch-lib";

# The menu row a click takes for a clash, or null when the menu has no row with that number.
def clash_choice($option):
  . as $clash
  | $clash.key.a as $a
  | $clash.key.b as $b
  | (if $clash.kind == "contract" and $clash.contract != null then
       $clash.contract.owner as $owner
       | (if $owner == $a then $b else $a end) as $other
       | [{choice: "split_at_contract", owner: $owner, contract: $clash.contract.interface.name},
          {choice: "serialize", first: $owner, then: $other}]
     else
       [{choice: "narrow", task: $a},
        {choice: "narrow", task: $b},
        {choice: "serialize", first: $a, then: $b}]
     end) as $menu
  | if $option > ($menu | length) then null else $menu[$option - 1] end;

# The timeline text of one event, as Format-Event writes it.
def describe:
  . as $e
  | if $e.type == "order_posted" then
      "order o-\($e.order) posted on \($e.machine): \($e.sentence) (\($e.witnesses | length) witnesses)"
    elif $e.type == "order_split" then
      (($e.tasks | map("t-\(.[0]) \(.[1].title)")) as $titles
       | "split into \($titles | length) task(s): \($titles | join("; "))")
    elif $e.type == "readback_posted" then
      "readback r-\($e.readback.id) on t-\($e.readback.task): \($e.readback.summary)"
    elif $e.type == "readbacks_reviewed" then
      (($e.verdicts // [] | map("r-\(.[0]) \(.[1] | tojson)")) as $verdicts
       | "reviewed: \($verdicts | join("; "))")
    elif $e.type == "lease_granted" then
      "claim t-\($e.task) granted (generation \($e.lease.generation), readback r-\($e.readback))"
    elif $e.type == "declared" then
      "declared t-\($e.decl.task): \($e.decl.footprint.files // [] | join(", "))"
    elif $e.type == "clash_found" then
      "CLASH c-\($e.clash) \($e.kind) on t-\($e.key.a) and t-\($e.key.b) at \($e.key.at | tojson)"
    elif $e.type == "clash_settled" then "clash c-\($e.clash) settled: \($e.choice | tojson)"
    elif $e.type == "clash_proposed" then "clash c-\($e.clash) proposed: \($e.choice | tojson)"
    elif $e.type == "clash_dissolved" then "clash c-\($e.clash) dissolved"
    elif $e.type == "clash_explained" then "clash c-\($e.clash) explained: \($e.explanation.consequence)"
    elif $e.type == "build_cleared" then "build cleared for t-\($e.task)"
    elif $e.type == "build_reported" then "built t-\($e.task) at \($e.head); gate run g-\($e.run)"
    elif $e.type == "witness_reported" then "witness w-\($e.witness) on g-\($e.run): \($e.outcome | tojson)"
    elif $e.type == "gate_closed" then "gate g-\($e.run) closed: \($e.verdict | tojson)"
    elif $e.type == "landed" then "LANDED t-\($e.task) as \($e.commit)"
    elif $e.type == "land_failed" then "land failed for t-\($e.task): \($e.reason)"
    elif $e.type == "lease_ended" then "lease on t-\($e.task) ended: \($e.why | tojson)"
    elif $e.type == "model_used" then
      "model \($e.model) for \($e.step): \($e.input_tokens) in, \($e.output_tokens) out"
    else $e.type
    end;

foreach (. // [])[] as $st (
  $origin;
  if . == null and $st.event.type == "order_posted" then $st.at else . end;
  . as $o
  | $st.event as $e
  | (if $o == null then 0 else ((($st.at - $o) / 1000) | round1) end) as $elapsed
  | ("+\($elapsed)s" | lpad(9)) as $when
  | ($st.seq | tostring | rpad(4)) as $seq
  | (if $settle > 0 and $e.type == "clash_found" then ($e | clash_choice($settle)) else null end) as $choice
  | {
      line: ($when + "  seq " + $seq + " " + ($e | describe)),
      settle: (if $choice == null then null else {clash: $e.clash, choice: $choice} end)
    }
)
