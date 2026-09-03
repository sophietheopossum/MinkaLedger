-- 0004: recurring payment chains.
--
-- A chain is money that passes through somewhere on the way, every time: current account to a
-- friend to a bookmaker. It is stored as one series PER HOP, so each leg keeps its own amount (a
-- fee taken on the way) and its own slot for a real payment to claim, and the hops are tied
-- together here. chain_id is the id of the FIRST hop's series and is set on every hop, the first
-- included, so "is this a chain member" is chain_id IS NOT NULL and "the whole chain" is one
-- equality. chain_seq is the hop's position, 0 for the first. head_id is deliberately NOT reused:
-- it means "this and future" splits of one series, and reports group by it.
--
-- Shared by construction: description, rrule, dtstart, until_on, weekend_rule and scenario_id.
-- series.end, series.rename and the override methods therefore apply to every hop of a chain at
-- once, an amount override travels through the hops as a delta so a fee stays a fee, and the
-- projection treats a scenario cancel of any hop as cancelling the chain.
ALTER TABLE series ADD COLUMN chain_id INTEGER REFERENCES series(id) ON DELETE CASCADE;
ALTER TABLE series ADD COLUMN chain_seq INTEGER CHECK ((chain_id IS NULL) = (chain_seq IS NULL));
CREATE INDEX series_chain ON series(chain_id);
