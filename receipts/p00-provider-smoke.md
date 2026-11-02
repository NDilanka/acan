# P00 provider receipts

Branch unit/p00, commit 699a0c7 (code as committed). The host is the base URL host from llm.toml. Times are IST, taken when each call started. Latency is the client's measured call time from the smoke output. No key values appear in this file or in any output.

## Live calls (six rows, final code)

| # | provider | host | mode | model id echoed by API | IST start | request id | source | latency ms | prompt tok | completion tok | result | pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | meta | api.meta.ai | --json | muse-spark-1.3 | 2026-10-28 22:54:56 | 281c8b68-29e3-4438-bd8b-73c6194580d6 | header x-request-id | 10232 | 25 | 638 | answer yes, count 5 | pass |
| 2 | meta | api.meta.ai | --tool | muse-spark-1.3 | 2026-10-28 22:55:10 | d17ee864-22d7-4482-82d5-8edfdfdcb2ec | header x-request-id | 4698 | 596 | 370 | get_route_status {"route":"worker-7"} | pass |
| 3 | baseten | inference.baseten.co | --json | meta-models/muse-spark-open-1026 | 2026-10-28 22:58:30 | 54ca701a1838e7580711a8cb90d0c947613c2 | header x-baseten-request-id | 5083 | 162 | 1381 | answer yes, count 5 | pass |
| 4 | baseten | inference.baseten.co | --tool | meta-models/muse-spark-open-1026 | 2026-10-28 22:58:37 | 54ca701a1838e8bfb7af89dd12c60ba8d5c1f | header x-baseten-request-id | 1360 | 685 | 142 | get_route_status {"route": "worker-7"} | pass |
| 5 | fireworks | api.fireworks.ai | --json | accounts/fireworks/models/muse-spark-open | 2026-10-28 22:55:46 | chatcmpl-0b89b3d46c064b39b729fd576b6b3b94 | header x-request-id | 27849 | 161 | 1308 | answer yes, count 5 | pass |
| 6 | fireworks | api.fireworks.ai | --tool | accounts/fireworks/models/muse-spark-open | 2026-10-28 22:56:15 | chatcmpl-d5637e7bd15f4b66bd5748acd415ef96 | header x-request-id | 3771 | 684 | 192 | get_route_status {"route": "worker-7"} | pass |

Rows 1 to 6 are the six required calls. Each passed on its first attempt, so no manual retry ran. No row is a stub, mock, or placeholder.

## Superseded Baseten attempts

These two calls ran before the x-baseten-request-id header was read. Their request ids came from the body. They passed, and the code path that produced them has since changed.

| provider | mode | IST start | request id | source | latency ms | prompt tok | completion tok | result | pass |
|---|---|---|---|---|---|---|---|---|---|
| baseten | --json | 2026-10-28 22:55:28 | chatcmpl-411f73a9950b41b0a2284b97ab63da71 | body id | 6647 | 162 | 1391 | answer yes, count 5 | pass |
| baseten | --tool | 2026-10-28 22:55:36 | chatcmpl-6c49f11db1f04817b59782800c544805 | body id | 1350 | 685 | 134 | get_route_status {"route": "worker-7"} | pass |

## Diagnostic call, not a receipt row

One Baseten chat call with max_tokens 8 returned HTTP 200. Only its response header names were recorded. They included X-Baseten-Request-Id, which is why the header list gained x-baseten-request-id. The call time was not recorded.

## Provider quirks

- Meta accepted muse-spark-1.3 and echoed the same id. No fallback to muse-spark-1.1 was needed.
- Meta used 638 completion tokens for a one-word answer. This is inferred, not measured. Hidden reasoning may be billed as completion tokens.
- All three providers accepted response_format json_schema in the --json mode. No provider needed the prompt fallback.
- All three providers returned a tool_calls entry for get_route_status with the default tool_choice. The client never sends tool_choice, so forced tool choice is untested.
- The client sends max_tokens only when a caller sets it. The smoke never sets it, so no live call sent it. The Meta docs call max_completion_tokens canonical, and that difference is not handled.
- Fireworks GET /models was not called. The chat endpoint accepted the model id and echoed it back.
- The Fireworks --json call took 27.8 s on its only attempt.

## P00F

Branch unit/p00f, commit 9ff0c51, which adds the token_limit_field setting. This closes the max_completion_tokens gap in Provider quirks above. Times are IST (+05:30 local clock), taken before each cargo run. Pass means the call returned a success status. Reply text is recorded as returned. The worktree llm.toml copy has token_limit_field = "max_completion_tokens" under meta, the same line llm.example.toml now carries. No key values appear in this file or in any output.

| # | provider | host | role | cap | field sent (configured) | model id echoed | IST start | request id | source | latency ms | prompt tok | completion tok | reply text | pass |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | meta | api.meta.ai | foreman | 64 | max_completion_tokens | muse-spark-1.3 | 2026-10-29 00:47:48 | 959e43bd-e7f9-4315-9c6c-a0c52c9e0f53 | header x-request-id | 3109 | 15 | 64 | empty | pass |
| 2 | baseten | inference.baseten.co | worker | 64 | max_tokens | meta-models/muse-spark-open-1026 | 2026-10-29 00:48:01 | 54ca701a183f2b54e7b3d8e8493cf2050f718 | header x-baseten-request-id | 1709 | 152 | 64 | empty | pass |
| 3 | fireworks | api.fireworks.ai | worker | 64 | max_tokens | accounts/fireworks/models/muse-spark-open | 2026-10-29 00:48:11 | chatcmpl-6d5f794e7888412dbf6b83c33f634286 | header x-request-id | 1898 | 151 | 40 | OK | pass |

- HTTP status: the smoke prints no status code. The client returns Ok only for a success status and retries 429 and 5xx, so each call returned a 2xx. This is inferred from the code path, not read off the wire.
- Field sent: the smoke prints the configured field. The wire body was not captured in a live call. The offline test token_cap_is_sent_under_the_configured_field_name checks the body that each setting produces.
- Rows 1 and 2 used all 64 completion tokens and returned no text. The cap probably ended generation before any reply text (inferred, not measured). Hidden reasoning is the likely cause, as in P00 row 1.
- Row 3 used 40 tokens, under the cap, so this call does not show the cap binding.
- Meta accepted max_completion_tokens with no error. Meta's answer to max_tokens was not tested in this unit.
- Open: a cap of 64 gives Meta and Baseten no text. A larger cap is needed to get a reply. Not tested here.
