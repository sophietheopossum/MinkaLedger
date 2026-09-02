-- =====================================================================
-- Give the core's own accounts an identity that survives a rename.
--
-- account.rename made every account name editable, including the three the
-- CORE creates for itself and then looks up again: the per-currency FX trading
-- accounts, the per-currency opening-balance counterweight, and the bucket the
-- importer files uncategorised rows into.  src/roles.rs now resolves those by
-- id (or, for conversion accounts, by a kind the schema makes unforgeable)
-- instead of by string.  This migration supplies the ids for books that
-- already have the accounts, so a book upgraded today is pinned BEFORE any
-- handler can rename anything -- db::migrate runs inside db::open, ahead of
-- the first request.
--
-- No table changes: book_meta is already the book's key/value store, and its
-- value column is TEXT under STRICT, so ids are cast on the way in and back
-- out again.  INSERT OR IGNORE keeps this idempotent and keeps it from
-- overwriting a pin roles.rs has already written.
-- =====================================================================

-- Matched on kind AS WELL AS name.  An ordinary expense account could have been
-- renamed onto the importer's string before this migration existed, but nothing
-- could have made it equity or conversion, so the kind is what stops a squat
-- being pinned as the real thing.
INSERT OR IGNORE INTO book_meta(key, value)
SELECT 'unclassified_account', CAST(id AS TEXT)
  FROM account
 WHERE name = 'Expenses:Unclassified' AND kind = 'expense';

-- One counterweight per currency.  MIN(id) rather than an arbitrary row: a book
-- that already grew a second counterweight (the exact bug this replaces) keeps
-- the original -- the one the existing opening transactions are posted against.
INSERT OR IGNORE INTO book_meta(key, value)
SELECT 'opening_equity.' || currency, CAST(MIN(id) AS TEXT)
  FROM account
 WHERE kind = 'equity' AND name = 'Opening balances (' || currency || ')'
 GROUP BY currency;
