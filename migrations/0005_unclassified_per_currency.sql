-- =====================================================================
-- One unclassified bucket per currency.
--
-- The importer files a row nobody categorised into a bucket, and until now the
-- book had exactly one: created in the display currency, pinned under the
-- single key 'unclassified_account'.  A posting is in its account's currency,
-- so that bucket could only ever balance a row in the same one -- an
-- uncategorised line of a EUR statement had nowhere to go.
--
-- src/roles.rs now keeps one per currency, pinned as
-- 'unclassified_account.<CUR>' in the shape of 'opening_equity.<CUR>'.  This
-- moves the existing pin under the key for the currency its account holds, so
-- the bucket a book already has goes on being the one for that currency --
-- under whatever name it has been given since -- instead of being orphaned
-- beside a new one.
--
-- No table changes, and the account itself is not touched: not renamed, not
-- re-currencied.  Only buckets created from here on are named with their
-- currency, 'Expenses:Unclassified (EUR)'.
-- =====================================================================

-- Joined on the account so the key carries ITS currency, which need not be
-- today's display currency.  The kind is checked for the reason 0003 checks
-- it: a pin left by a deleted bucket must not be carried over onto whatever
-- now holds that id.
INSERT OR IGNORE INTO book_meta(key, value)
SELECT 'unclassified_account.' || a.currency, m.value
  FROM book_meta m
  JOIN account a ON a.id = CAST(m.value AS INTEGER)
 WHERE m.key = 'unclassified_account' AND a.kind = 'expense';

DELETE FROM book_meta WHERE key = 'unclassified_account';
