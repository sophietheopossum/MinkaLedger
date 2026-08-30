-- Free-form links between transactions (req 10, second form).
--
-- A JOURNEY IS A CONTAINER; THIS IS A GRAPH. journey/journey_member model an ordered chain with
-- named roles, which suits a transfer you plan in advance. This models the other thing: "that
-- payment led to this one", asserted between any two transactions, after the fact, with no
-- container to create first and no order to get right. Both survive; they answer different
-- questions and neither is a migration of the other.
--
-- DIRECTED, TRAVERSED UNDIRECTED. from -> to records which came first, so a chain can be drawn
-- with arrows. Following a thread ignores direction, so it does not matter which end you start
-- from. A 2-cycle (A->B and B->A) is therefore harmless rather than forbidden -- reachability
-- dedupes it -- and forbidding it would need a cross-row constraint SQLite cannot express.
--
-- CASCADE both ways: a link is a statement ABOUT two transactions and cannot outlive either.
-- Deleting a payment must not leave an edge pointing at nothing.
CREATE TABLE txn_link (
  from_txn_id INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  to_txn_id   INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  note        TEXT,
  created_on  TEXT NOT NULL,
  PRIMARY KEY (from_txn_id, to_txn_id),
  CHECK (from_txn_id <> to_txn_id),
  CHECK (created_on = date(created_on))
) STRICT, WITHOUT ROWID;

-- The reverse index is what makes "what links TO this payment" as cheap as the forward direction,
-- which matters because traversal walks both.
CREATE INDEX txn_link_to ON txn_link(to_txn_id);
