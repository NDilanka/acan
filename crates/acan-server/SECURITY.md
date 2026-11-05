# Event log integrity

Event-store format 1 authenticates each row with HMAC-SHA256 over the domain
`acan-event-log-v1`, the previous MAC, the row sequence (big-endian i64), and
canonical JSON (RFC 8785). A transactional head records the sequence and MAC of
the complete log. Opening the store and every `load_from`, including resumed
reads, verify the whole chain before returning events. Appends serialize through
SQLite's write lock and commit their rows and head together.

The random 32-byte key lives beside `world.db` as `world.key`, created without
overwriting another file and with mode 0600 on Unix. Keep it private, out of git,
and back it up with the database. Existing installations with a tracked
`.acan/.gitignore` should add `world.key` to it. A missing, malformed, symlinked,
or group/world-readable key fails closed. Never delete a key to repair a log.

For compatibility, the first upgrade seals the existing unsigned rows in one
transaction. This is trust on first use: it cannot detect tampering that predates
the upgrade. Upgrade only a trusted local database, not a repository-supplied
log. Once sealed, unsigned rows and mismatched MACs are rejected; the store never
silently rekeys or reseals. A crash after key publication but before the seal
commits also fails closed; restore a trusted database/key backup.

This protects against event edits, insertion, reordering and ordinary truncation
by a database-only writer. It does not protect against a writer who can also
read/replace the key, or rollback of the database and its head to an older valid
snapshot. An external monotonic checkpoint would be needed to detect rollback.
Raw `Origin::replay`/`Replica::ingest` and redacted WebSocket events remain
unauthenticated formats: use only a trusted origin and authenticated TLS for
remote transport. A per-store HMAC is not a public replica signature, and its
secret must never be distributed to clients.
