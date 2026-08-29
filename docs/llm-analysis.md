# Analysing the book with a language model

The ledger has always been *exportable*. `export.bundle` hands over every posting with a preamble
explaining double entry and minor units, and that is enough to be readable.

It is not enough to be **analysable**, and the difference is worth being precise about, because
"just send it the data" is the obvious answer and it is the wrong one.

## What goes wrong when you send the data

Give a model four thousand ledger lines and ask what you spend on groceries. It will:

- **get the arithmetic wrong** — summing hundreds of integers is the thing it is worst at, and it
  will not tell you it is unsure;
- **not know what is committed** — rent and a takeaway are the same shape in the data, so "cut your
  spending" arrives without knowing which half you could actually cut;
- **have no baseline** — "you spent £340 on groceries" is not a finding without "you usually spend
  £288";
- **annualise wrong** — a £30 four-weekly gym membership costs £32.50 a month, not £30, because it
  is billed thirteen times a year. Times-twelve is 8% under, every time;
- **compare a part month against whole ones** — August on the 12th looks like a spending collapse;
- **run out of context** and analyse the prefix it managed to read, without saying so.

None of that improves if you send more data. It improves if you send **fewer numbers, already
computed**.

## The three methods

### `analysis.brief`

The computed picture. Balances, what recurs and its true monthly cost, complete months of history,
medians and how far the latest month sits from them, largest expenses, the forward outlook, and —
the part that matters most — `limits`.

Everything is derived in the core, in integer arithmetic, by the same projection engine that draws
the forecast chart. The document says how each figure was derived, because a model that cannot see
a derivation will helpfully redo it.

```
{"id":1,"method":"analysis.brief","params":{"as_of":"2026-08-29","months":6}}
```

Pass `path` to write it to a file instead of returning it.

**`limits` is not boilerplate.** It states what the book cannot tell you — how much spending has no
category, how far back the history reaches, how many complete months the median rests on, whether a
foreign currency has a stale rate, whether anything recurring is recorded at all. Several entries
invalidate whole sections when they fire. An analysis that does not know its own blind spots is
worse than none, because it is confident.

### `analysis.query`

Read-only SQL, for everything the brief summarises away.

```
{"id":2,"method":"analysis.query","params":{"sql":"SELECT payee, COUNT(*), SUM(amount_minor) FROM v_ledger_line WHERE account='Groceries' GROUP BY payee ORDER BY 3 DESC","limit":50}}
```

The connection is opened `SQLITE_OPEN_READ_ONLY`, so a write is refused by SQLite itself rather
than by a filter that has to be right. Results are capped and a truncated result says so — a
silently short answer is the worst outcome here. A runaway join is interrupted by a step budget
rather than wedging the core.

### `analysis.schema`

Every table and view with its DDL **and its comments**. The comments carry the reasoning the column
names do not — they are the difference between a reader that knows `occurrence_on` is a slot
identity and one that treats it as a date.

## Two ways to use it

**Paste.** Open the window, press *brief*, then *Copy for a model*, and paste into a chat. The
screen and the clipboard are the same computation, so a disagreement between you and the model is
about judgement rather than about who added up wrong.

**Live.** The core is already a tool server: NDJSON over stdio, one JSON object per line, exactly
the shape an agent harness wants. Point one at it and let it ask its own questions.

```sh
minka-ledger --db ~/.local/share/minka-ledger/book.db
```

`analysis.tools` returns a machine-readable description of the callable surface and a suggested
order — brief first, then schema, then queries. Handing that to a model up front is the difference
between one that asks for a data dump and one that asks a question.

## Size

At 399 transactions, on the same book:

| | bytes |
|---|---|
| `export.bundle` | 238,307 |
| `analysis.brief` | 14,943 |

The bundle grows with every transaction. The brief grows with accounts × months, so it stays
roughly this size on a book with ten years in it — which is what makes repeated analysis possible
rather than a single shot that fills the context.
