---- MODULE StoppedLane ----
\* One order's tasks. Sources: clash.rs:824-852 (build_clear), repair.rs:329-345 and
\* gate.rs:196-216 (receipt close), world.rs:1083-1091 (lease end on stop), lane.rs:425 (claim).
\* Fixed = TRUE is the proposed fix.
EXTENDS Naturals
CONSTANTS Tasks, Fixed, Mutant        \* Mutant \in {"none", "keep_lease", "unstop"}
VARIABLES stage, lease, stopped
vars == <<stage, lease, stopped>>
Claimed(t) == stage[t] \in {"declared", "building", "gating"}
TypeOK == stage \in [Tasks -> {"open","declared","building","gating","landed"}]
          /\ lease \in [Tasks -> BOOLEAN] /\ stopped \in BOOLEAN
Init == stage = [t \in Tasks |-> "open"] /\ lease = [t \in Tasks |-> FALSE] /\ stopped = FALSE
Claim(t) == ~stopped /\ stage[t] = "open"                          \* lane.rs:424-427
            /\ stage' = [stage EXCEPT ![t] = "declared"] /\ lease' = [lease EXCEPT ![t] = TRUE]
            /\ UNCHANGED stopped
Clear(t) == stage[t] = "declared" /\ (Fixed => ~stopped)
            /\ stage' = [stage EXCEPT ![t] = "building"] /\ UNCHANGED <<lease, stopped>>
Built(t) == ~stopped /\ stage[t] = "building"                      \* lane.rs:762
            /\ stage' = [stage EXCEPT ![t] = "gating"] /\ UNCHANGED <<lease, stopped>>
Pass(t)  == stage[t] = "gating" /\ stage' = [stage EXCEPT ![t] = "landed"]
            /\ lease' = [lease EXCEPT ![t] = FALSE] /\ UNCHANGED stopped
Fail(t)  == stage[t] = "gating" /\ stage' = [stage EXCEPT ![t] = "declared"]
            /\ UNCHANGED <<lease, stopped>>                         \* lease renewed
Stop(t)  == stage[t] = "gating" /\ stopped' = TRUE                 \* receipt over budget
            /\ IF Fixed
               THEN /\ stage' = [u \in Tasks |-> IF Claimed(u) THEN "open" ELSE stage[u]]
                    /\ lease' = IF Mutant = "keep_lease" THEN lease ELSE [u \in Tasks |-> FALSE]
               ELSE /\ stage' = [stage EXCEPT ![t] = "declared"] /\ UNCHANGED lease
Release(t) == lease[t] /\ stage[t] # "gating"                      \* the agent's choice
            /\ stage' = [stage EXCEPT ![t] = "open"] /\ lease' = [lease EXCEPT ![t] = FALSE]
            /\ UNCHANGED stopped
Unstop == Mutant = "unstop" /\ stopped /\ stopped' = FALSE /\ UNCHANGED <<stage, lease>>
Next == Unstop \/ \E t \in Tasks :
          Claim(t) \/ Clear(t) \/ Built(t) \/ Pass(t) \/ Fail(t) \/ Stop(t) \/ Release(t)
Spec == Init /\ [][Next]_vars
        /\ \A t \in Tasks : WF_vars(Clear(t)) /\ WF_vars(Pass(t) \/ Fail(t) \/ Stop(t))
NoBuildWhenStopped == stopped => \A t \in Tasks : stage[t] # "building"
LeaseMeansClaimed  == \A t \in Tasks : lease[t] <=> Claimed(t)
LeasesEnd          == stopped ~> \A t \in Tasks : ~lease[t]
StopIsFinal        == [][stopped => stopped']_vars
====
