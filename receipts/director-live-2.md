# Director live receipt 2

Status: admitted. One live director call over a real landed receipt returned a tour that admission accepted. The tour is one step on the signup check, captioned `preserved.open`.

This receipt also records the call before it, which the director's output cap no longer stopped and whose tour was not admitted. The two calls differ by one catalog change, described below.

Code under test: branch `unit/director-cap` on wip at `0de7159`. It holds `256745f` (the director cap and the trace of an unusable reply), `fa17457` (the structured logs) and `0de7159` (the `preserved.open` and `preserved.fixed` captions, catalog `captions-5`). The live test was a scratch harness that is not in any commit.

## What was real

- The gate. Order `o-0` had one confirmed scope. A candidate commit that adds a note file to the demo was run through the real witness probes in Docker, against the pinned acan-demo checkout at `f919621`. The candidate landed on the protected branch.
- The receipt. Its plant view was built from the landed receipt by the same builder that serves `GET /api/orders/{order}/plant-view`. The view has 1 item.
- The director call. One live call per run on the Meta route for the director, with the view as context. The route is `routing.director` in the example routing file: provider `meta`, default model `muse-spark-1.3`.

## What was scripted

- The order split, the scope and the worker steps were driven by a stub client. The stub also answered the foreman's scope draft, which failed twice and retried. The retries do not change the gate result.
- The stub replaced the foreman route and the worker route. Only the director call was live.

## Director calls

| # | run | provider | model | output cap | temperature | request id | prompt tok | completion tok | latency | result |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | rerun 1, tracing off | meta | muse-spark-1.3 | 16000 | 0.0 | 5e79a41b-3075-4e0e-bf27-060bde0e5a36 | 4263 | 3901 | 46.2 s | reply, not capped; no tour admitted; plant view had no tour |
| 2 | rerun 2, tracing on | meta | muse-spark-1.3 | 16000 | 0.0 | 6ec7118b-a1ca-49a8-b8f0-ceb3baf5bc84 | 4333 | 6422 | 77.8 s | reply, not capped; admitted |

The reply's model echo was `muse-spark-1.3` in both calls. The completion counts include any hidden reasoning tokens. The split between reasoning and tour text is not recorded.

Both calls were made on the same landed receipt shape. The receipt hash differs between the two runs because each run lands a new candidate commit.

## Admitted tour (call 2)

- Mode `directed`, model `muse-spark-1.3`, catalog `captions-5`.
- One step, on item `k-signup-body`. The item's class is `preserved` and its state is `open`.
- Camera: machine `auth`, zoom `machine`.
- Caption: template `preserved.open`, no slots, no evidence references.
- Stage marks the view draws: `crates`, `ghost`, `must_keep_pin`, `order_plate`.
- Bindings: one entry, `k-signup-body`, `typed_slot_bindings` empty.

## Why call 1 was not admitted

The reply of call 1 was not kept, and the admission reason was not logged in that run. The inference comes from the catalog and the view, not from the reply text.

Every item in this receipt is `preserved` and `open`, the normal state of a passing check. The `captions-4` catalog had templates for preserved items in the bounced, residue and stopped states only. A tour step on an open preserved item therefore had no template that fits it, and admission refuses such a step. Call 2 was the same kind of reply on `captions-5`, and it was admitted. That supports the inference, but it does not prove the reply of call 1 contained that step.

The fix is commit `0de7159`. It adds `preserved.open` and `preserved.fixed`, and it moves the catalog to `captions-5`. A new test checks that every class and state has a template.

## Plant view timing

- The plant view waits 20 s for the director. Its first answer was served with no tour, because the call had not finished. The call kept running and its answer was cached for this receipt.
- The director finished at 77.8 s. The test's second fill of the same receipt received the admitted tour from the cache.

## Fallback

The deterministic fallback has no tour for this receipt. The only item is `preserved` and `open`, and the fallback skips such items on purpose. So a clean receipt serves no tour when the director does not answer. Whether the fallback should visit passing items is a product decision that this receipt does not settle.

## Gate and hashes

```
sha256:925c3f2ed0ced1be11b62fc73ce57d65f17d6a5bba8f2301f0057b1e4c1d4940  receipt hash of o-0, call 2
sha256:7be4e1b12f51d718f71f9735af141e66fde0f76646e8e6322b6c410ab446bacf  receipt hash of o-0, call 1
```

The protected head after landing was `abcd9d40` in call 2 and `95202ca9` in call 1. The view's `gate_state` reads `prepared`, which means the merged commit is prepared. The landed task was observed in both runs.

## Log lines of call 2

- The director replied, with the receipt, model, request id, token counts and latency.
- Two scope-draft retries from the stub.
- One warning that the director had not answered within 20 s, followed by the empty fallback.

No admission refusal appears in call 2.

## Cost

Calls 1 and 2 together: 8596 prompt tokens and 10323 completion tokens. No price is recorded here.

## Not in this receipt

- No retry was made by the test. The client's own HTTP retries, if any, are not in the log.
- The tour is admitted by the rules of the view. It was not judged by a human, and no candidate was reviewed for the demo.
- The fallback has no tour for a clean receipt, as described above.
- Only the director was tested live. The foreman, worker and vision routes were stubbed or not run.
- The view was built from one receipt of one order, on one landed commit.
