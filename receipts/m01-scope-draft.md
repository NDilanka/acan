# M01 provider receipts

Branch unit/m01. Times are IST (+05:30), taken from the shell right before the cargo run. No key value appears in this file or in any output.

## Live call (one row)

| # | provider | host | role | cap | field sent (configured) | model id echoed | IST start | request id | source | latency ms | prompt tok | completion tok | result | pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | meta | api.meta.ai | foreman | 8000 | max_completion_tokens | muse-spark-1.3 | 2026-10-29 18:31:31 | b1ac0ce9-64b7-447a-8c5f-106d78eb1c60 | header x-request-id | not recorded by the test | 6604 | 5277 | scope draft decoded as a ScopeBody and accepted by Acan | pass |

Command: `ACAN_LIVE_PROOF=1`, `ACAN_DEMO=<demo checkout>`, then `cargo test -p acan-server --locked --test model_jobs muse_spark_drafts_the_scope_of_the_rate_limit_order -- --nocapture --exact`. Result: 1 passed, 22.25 s. Only one live call was made; an earlier run used a wrong test filter and ran 0 tests, so it made no call.

Order: "Add per-user rate limiting to link creation and keep the login flow unchanged". The route table, fixtures, limits and machines come from a read-only read of <demo checkout> (`.acan/probes/core.json`, `.acan/app.json`, and a scan of the repo). The demo repo was not written to.

Draft as returned (summarized):
- may: one item on POST /api/links, provenance user, machines ratelimit, links, store. Acceptance is a window predicate, 5 allowed per 60000 ms per authenticated user, with 429 counted as rejected. The example is a target with status 429 at step create-link.
- must_keep: POST /api/login, equals_base predicate with a fresh anchor, machines auth, store.
- open: one choice on ratelimit, "What per-user limit applies to link creation?", options 5-per-min, 10-per-min, 20-per-hour.

Notes:
- Completion tokens (5277) are below the 8000 cap, so the cap did not cut the reply. The reply parsed on the first attempt.
- The live call's wire body was not captured. The field name is the configured one (llm.toml, meta, token_limit_field = max_completion_tokens). The offline test `a_draft_request_reaches_the_provider_with_a_generous_output_cap` checks the body that reaches the provider under the default field name (max_tokens = 8000).
- No tool use is claimed. No tool call was made or recorded for this draft.
