# minka-ledger

A forecast-first personal ledger. Double-entry core, RFC 5545 recurrence, and a projection that is a
pure function over rules rather than a table of persisted guesses.

## Why it is shaped like this

- **Double-entry from day one.** Every transaction is a set of postings summing to zero, per
  currency. Retrofitting this is a rewrite, and it is what makes tracing money across accounts fall
  out for free instead of being a feature.
- **Integer minor units.** Amounts are signed `i64` pence/cents/satoshi. Never floats. `money.rs` is
  the only place rounding is implemented.
- **The forecast is derived, never stored.** Generated occurrences are computed and thrown away.
  That is what makes a what-if scenario an argument to a function rather than a second database.
- **SQLite, one file.** Backup is `cp`. Export for analysis is `SELECT`.

## Layout

    migrations/0001_init.sql   23 tables, 6 views. STRICT throughout.
    src/money.rs               integer money + the single rounding rule
    src/main.rs                NDJSON stdio server

## Protocol

Speaks MinkaLink's NDJSON shape verbatim, so a Quickshell frontend reuses the same client:

    in   { "id": n, "method": "...", "params": {...} }   expects a response
         { "method": "...", "params": {...} }            fire-and-forget
    out  { "id": n, "result": ... } | { "id": n, "error": { "code", "message" } }
         { "event": "...", "payload": ... }              broadcast

Malformed lines are ignored rather than fatal, and a panicking handler returns an error instead of
ending the session.

## Try it

    cargo build
    printf '%s\n' '{"id":1,"method":"health.ping"}' | ./target/debug/minka-ledger

## Status

Build step 1 of 9 (schema, money, RPC skeleton). Steps 2-9: accounts and manual entry, recurrence
and forecast, journeys, multi-currency, interest, CSV import, export, QML frontend.

Deferred deliberately, unblocked by the schema: crypto Section 104 pooling, and income
categorisation for self-assessment. Both are pure functions over history that a ledger already
stores, so neither needs a schema change.
