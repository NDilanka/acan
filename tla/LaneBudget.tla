---- MODULE LaneBudget ----
\* One lane's repair budget. Cap and rounds follow repair.rs classify (crates/acan-core/src/repair.rs):
\* each round bounces the first MaxEpisodes bounceable items, and the rest stay open for a later round.
\* Fixed = TRUE is the proposed fix: a held ruling settles residue. Mutant \in {"none", "off_by_one", "unstop"}.
EXTENDS Naturals, FiniteSets
CONSTANTS Items, MaxEpisodes, RepairRounds, Fixed, Mutant
VARIABLES ep, rounds, stopped, ruled
vars == <<ep, rounds, stopped, ruled>>
Pending(s) == s \in {"open", "bounced"}
Min(x, y) == IF x < y THEN x ELSE y
Init == ep = [i \in Items |-> "none"] /\ rounds = 0 /\ stopped = FALSE /\ ruled = {}
Rule(i) == ep[i] = "residue" /\ i \notin ruled
           /\ ruled' = ruled \cup {i} /\ UNCHANGED <<ep, rounds, stopped>>
\* Items in B fail in a bounce class, items in R in a residue class, the rest pass.
\* K is the batch this round bounces: a subset of the bounceable items, of the size the cap allows.
\* The code takes the fewest bounces first, then identity; any batch of that size is allowed here.
Receipt(B, R, K) == ~stopped /\ B \cap R = {} /\
  LET fails(i) == i \in B \cup R
      s1(i) == IF ~fails(i) THEN ep[i]
               ELSE IF i \in R /\ ep[i] \in {"none","open","bounced","fixed","ruled"}
                    THEN "residue"                                 \* repair.rs:516-524
               ELSE IF ep[i] \in {"none","fixed","ruled"} THEN "open" ELSE ep[i]
      s2(i) == IF ~fails(i) /\ Pending(s1(i)) THEN (IF i \in ruled THEN "ruled" ELSE "fixed")
               ELSE IF Fixed /\ ~fails(i) /\ s1(i) = "residue" /\ i \in ruled THEN "ruled"
               ELSE s1(i)                                          \* today: residue stays
      bounce == {i \in Items : Pending(s2(i))}
      limit  == IF Mutant = "off_by_one" THEN RepairRounds + 1 ELSE RepairRounds
      stop   == bounce # {} /\ (rounds >= limit \/ K = {})
  IN /\ K \subseteq bounce
     /\ Cardinality(K) = Min(MaxEpisodes, Cardinality(bounce))
     /\ IF stop
        THEN /\ ep' = [i \in Items |-> IF Pending(s2(i)) THEN "stopped" ELSE s2(i)]
             /\ stopped' = TRUE
             /\ UNCHANGED <<rounds, ruled>>
        ELSE /\ ep' = [i \in Items |-> IF i \in K THEN "bounced" ELSE s2(i)]
             /\ rounds' = IF K = {} THEN rounds ELSE rounds + 1
             /\ UNCHANGED <<stopped, ruled>>
Unstop == Mutant = "unstop" /\ stopped /\ stopped' = FALSE /\ UNCHANGED <<ep, rounds, ruled>>
Next == Unstop \/ (\E B, R, K \in SUBSET Items : Receipt(B, R, K)) \/ (\E i \in Items : Rule(i))
Spec == Init /\ [][Next]_vars /\ WF_vars(Receipt({}, {}, {})) /\ \A i \in Items : WF_vars(Rule(i))
ResidueClears  == \A i \in Items : ep[i] = "residue" ~> (ep[i] = "ruled" \/ stopped)
RoundsBounded  == rounds <= RepairRounds
\* A stop at round 0 needs an empty batch, which happens only when MaxEpisodes = 0 (repair.rs classify).
StopAfterRound == stopped => rounds > 0
StopIsFinal    == [][stopped => stopped']_vars
====
