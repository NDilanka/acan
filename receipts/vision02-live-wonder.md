# VIS02 live Muse Wonder receipt

Branch unit/batch3, commit 45aad7b (the live test change). Code under test: 98e173e plus 45aad7b. Times are IST. The API keys came from the environment variables that llm.toml names. No key value appears in this file or in the test output, and the test log was checked for the key values: none appeared.

## Inputs

The same dashboard pair as the VIS01 receipt, rendered from the acan-demo app at f919621. The first 8 hex digits of each sha256 are the same as in that receipt: baseline 42d1c604, keep 33e1a7b1, break bf117a48. The pair folder is kept outside the repository.

Must-keep items sent: total-clicks-column ("The total clicks column stays") and summary-line ("The summary line stays").

Pages were rendered with the installed Microsoft Edge through the witness's host renderer. Scripts were off by the page policy, network was refused, and the viewport was 1280 x 800.

The candidates are hand-made, as in VIS01. This is not an o-7 take: no o-7 candidate exists yet.

## Calls

| # | file | look | provider | model | request id | prompt tok | completion tok | latency ms | verdict | pass |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | keep.html | first | meta | muse-spark-1.3 | 4d44ac94-2874-42b4-a5a8-8c1cd7b98aa6 | 2738 | 500 | 5005 | consistent | pass (expected consistent) |
| 2 | break.html | first | meta | muse-spark-1.3 | 6063c17c-0e8c-4921-8941-f8a97dd386a6 | 2738 | 559 | 4438 | inconsistent | pass (expected inconsistent) |
| 3 | break.html | second | meta | muse-wonder-1.0 | 61af6db4-d229-443d-931c-06abf905e814 | 2738 | 222 | 3486 | inconsistent | checked, no fallback |

Call 3 runs because call 2 found the candidate inconsistent. The second look sends no temperature and sets max_tokens to 2000. Its 222 completion tokens are below that cap.

Reasons the models gave:
- keep, first look: "The total-clicks column and the summary line both remain visible in the candidate."
- break, first look: "The total-clicks-column is missing in the candidate though the summary-line remains."
- break, second look: "The total clicks column is missing in the candidate table, though the summary line remains."

Request shape: one user turn with the text part BASELINE, the baseline PNG, the text part CANDIDATE, the candidate PNG, then the route and the must-keep list. The second look uses the same request as the first, with no first verdict in it. The routing for the second look is the `wonder` row in llm.toml: provider meta, model muse-wonder-1.0. The live calls did not capture the wire body, so the field name that reached the provider is not measured here. The acan-llm offline tests check the body each field setting produces.

## Command

    ACAN_LIVE_PROOF=1, ACAN_VISION_PAIR=<folder holding baseline.html, keep.html and break.html>
    cargo test -p acan-server --locked --lib the_live_vision_call_reads_the_dashboard_pair -- --nocapture

Environment: CARGO_INCREMENTAL=0, CARGO_PROFILE_DEV_DEBUG=0, CARGO_PROFILE_TEST_DEBUG=0. Result: exit 0, 1 passed, 39.81 s. The log was kept outside the repository. It ended at about 00:18 IST on Nov 3.

Cost in tokens: 8214 prompt, 1281 completion, 3 calls. No price is recorded here.

## Not in this receipt

- Nothing was run against a real o-7 candidate. The candidates are hand-made, and the verdict covers only the first 1280 x 800 pixels of the dashboard page.
- The Docker witness capture path was not run for this pair.
- All three verdicts agree with the expected results, but Muse Wonder was not tested on a pair the first look called unclear.
- The vision witness is advisory. Its verdicts never set pass or fail.
