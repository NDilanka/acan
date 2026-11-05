# Driver reply integrity

Each host operation carries a fresh random UUID nonce. Both bundled drivers echo it,
including on failure. The host rejects missing, incorrect and stale nonces before
recording any observation. Nonces are not sent to the app over HTTP or its environment.
There is no fallback to the old protocol.

This is reply correlation, not full authentication against a same-uid attacker.
The driver and app still share a uid and PID namespace. On a runtime that permits
the app to read `/proc/1/fd/0` or inspect driver memory, it could learn the current
nonce and forge a matching reply on `/proc/1/fd/1`. Nonces prevent blind injection
and replay, but do not close that stronger variant of witness-002.

Follow-ups: separate the driver and app into different non-root uids, and move the
reply channel to a driver-owned private socket. Dropping all capabilities and
forbidding uid 0 currently prevents the driver from switching its child to another
uid, so that needs a container launch design change rather than a spawn flag.
Runtime isolation tests require Docker; protocol tests do not.
