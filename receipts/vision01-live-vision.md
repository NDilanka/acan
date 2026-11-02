# VIS01 live vision receipts

Branch unit/vision01, commit 39c7e1d (test added). Code under test: 9a2eab9..ee4d904, on top of main 2773515. The live calls ran on 39c7e1d. Commits after it on this branch were not run live again. Times are IST. The API key came from the local environment file that git ignores. No key value appears in this file or in any output.

## Inputs

Baseline: the acan-demo app at commit f919621, working tree clean. A capture ran on the host, not in the Docker witness. It used the demo's own test helpers to start the app in process, sign up alice_01, create two links, click the first link twice, then GET /dashboard as alice. This is the dashboard-session probe's signed-in steps, with a second link and clicks added.

| file | what it is | must-keep result |
|---|---|---|
| baseline.html | real app output, 1264 bytes | reference |
| keep.html | baseline plus a "Last 24 h" column after Clicks, same numbers | both items hold |
| break.html | baseline with the Clicks column removed, summary line kept | total clicks column missing |

keep.html and break.html were made by string edits in the capture script. They are hand-made candidates, not agent builds.

This is not an o-7 take. No o-7 candidate exists yet. No commit in acan-demo adds a 24-hour column (checked main, unit/d01, unit/d01p and unit/d01q). Re-run this check on the real o-7 candidate when one exists.

Must-keep items sent: total-clicks-column ("The total clicks column stays") and summary-line ("The summary line stays").

Screenshots: rendered with the installed Microsoft Edge through the witness's host renderer. Scripts were off by the page policy, network was refused, the profile was throwaway, and the viewport was 1280 x 800. Screenshots are not part of the repository. The first 8 hex digits of each sha256: baseline 42d1c604, keep 33e1a7b1, break bf117a48.

## Calls

| # | file | provider | model | request id | prompt tok | completion tok | latency ms | verdict | expected | pass |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | keep.html | meta | muse-spark-1.3 | bf79498d-deda-414e-a96f-72dac19bff53 | 2738 | 498 | 16700 | consistent | consistent | pass |
| 2 | break.html | meta | muse-spark-1.3 | f66cfc37-6a9d-4862-9f2f-460a398d90c9 | 2738 | 385 | 28453 | inconsistent | inconsistent | pass |

Start times were not recorded by the test output, so the table has none. Latency is the client's measured call time.

Reasons the model gave:
- keep: "The total clicks column and summary line both remain visible in the candidate."
- break: "The total clicks column is missing in the candidate table."

Request shape: one user turn with the text part BASELINE, the baseline PNG, the text part CANDIDATE, the candidate PNG, then the route and the must-keep list. System prompt: the vision prompt in judge.rs. max_tokens 1200. temperature 0.0.

Temperature 0.0: both calls returned HTTP 200 with a verdict, so the provider accepted it. It was not dropped.

Notes:
- Completion tokens (498 and 385) are below the 1200 cap, so the cap did not cut either reply.
- The cap is sent under the meta provider's token_limit_field setting. The example config sets max_completion_tokens. The live calls did not capture the wire body, so the field name that reached the provider is not measured here. The acan-llm offline tests check the body each field setting produces.
- No tool use is claimed. No tool call was made or recorded.

## Command

    ACAN_LIVE_PROOF=1, ACAN_VISION_PAIR=<folder holding baseline.html, keep.html and break.html>
    cargo test -p acan-server --locked --lib the_live_vision_call_reads_the_dashboard_pair -- --nocapture

Environment: CARGO_BUILD_JOBS=2. Result: exit 0, 1 passed, 60.44 s. The log was kept outside the repository.

Cost in tokens: 5476 prompt, 883 completion, 2 calls. No price is recorded here.

## Not in this receipt

- Muse Wonder was not called.
- The Docker witness capture path was not run for this pair.
- Nothing was run against a real o-7 candidate.
- The verdict covers only the first 1280 x 800 pixels of the dashboard page.
- The vision witness is advisory. Its verdicts never set pass or fail.
