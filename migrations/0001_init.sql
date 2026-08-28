-- =====================================================================
-- MinkaLedger v1 schema.  SQLite >= 3.37 for STRICT (local: 3.53.4, verified).
-- Every statement below was executed clean, and every CHECK / partial-unique
-- guard was tested to fire.  23 tables, 6 views.
--
-- STRICT is load-bearing everywhere: it is what stops an INTEGER minor-unit
-- column silently accepting a float.  (Verified: inserting 10.5 into
-- posting.amount_minor errors "cannot store REAL value in INTEGER column".)
-- =====================================================================
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- ============ 0. BOOK CONSTANTS ============
-- Curated, never inferred from an API: a wrong minor_digits moves every
-- amount in that currency by 100x silently.
CREATE TABLE currency (
  code         TEXT PRIMARY KEY,
  minor_digits INTEGER NOT NULL,
  name         TEXT NOT NULL,
  CHECK (length(code) = 3),
  CHECK (minor_digits BETWEEN 0 AND 8)
) STRICT;

-- display_currency, schema_version, rate_derivation_version.
CREATE TABLE book_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

-- ============ 1. CHART OF ACCOUNTS ============
-- One currency per account, NOT NULL. UNIQUE(id,currency) is the target of
-- posting's composite FK, which makes "posting currency == account currency"
-- a database guarantee. kind='conversion' is GnuCash trading accounts:
-- core-written only (system=1); the UI and importer must refuse to post here.
-- parent_id gives the account tree (card rate buckets, rollups).
CREATE TABLE account (
  id        INTEGER PRIMARY KEY,
  name      TEXT NOT NULL UNIQUE,
  kind      TEXT NOT NULL CHECK (kind IN ('asset','liability','income','expense','equity','conversion')),
  currency  TEXT NOT NULL REFERENCES currency(code),
  parent_id INTEGER REFERENCES account(id),
  system    INTEGER NOT NULL DEFAULT 0 CHECK (system IN (0,1)),
  closed    INTEGER NOT NULL DEFAULT 0 CHECK (closed IN (0,1)),
  CHECK (kind <> 'conversion' OR system = 1),
  UNIQUE (id, currency)
) STRICT;
CREATE INDEX account_parent ON account(parent_id);

-- ============ 2. FLOWS (req 5, 10) ============
CREATE TABLE journey (
  id        INTEGER PRIMARY KEY,
  label     TEXT NOT NULL,
  opened_on TEXT NOT NULL,
  closed_on TEXT
) STRICT;

-- ============ 3. SCENARIOS (req 8) ============
-- ui_selected is FRONTEND STATE ONLY. forecast() takes active scenarios as an
-- argument and never reads this column.
CREATE TABLE scenario (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  note        TEXT,
  ui_selected INTEGER NOT NULL DEFAULT 0 CHECK (ui_selected IN (0,1))
) STRICT;

-- ============ 4. RECURRENCE (req 1, 2, 8) ============
-- rrule is the RRULE body only, no DTSTART line. COUNT is banned: RFC 5545
-- counts generated occurrences BEFORE EXDATE removal, so skipping one
-- instalment of "12 payments" silently yields 11. Bound with until_on
-- (INCLUSIVE per RFC 5545 3.3.10).
-- head_id: "this and future" edits SPLIT the series. Every report groups by
-- COALESCE(head_id,id) -- grouping by id silently fragments totals.
-- weekend_rule/holiday_cal: RRULE cannot say "not on a bank holiday"; applied
-- AFTER expansion and moves the VALUE date only, never the slot identity.
-- supersedes_id: a scenario row that SUPPRESSES a baseline row (cancel Netflix).
CREATE TABLE series (
  id                  INTEGER PRIMARY KEY,
  head_id             INTEGER REFERENCES series(id),
  description         TEXT NOT NULL,
  rrule               TEXT NOT NULL,
  dtstart             TEXT NOT NULL,
  until_on            TEXT,
  weekend_rule        TEXT NOT NULL DEFAULT 'none'
                        CHECK (weekend_rule IN ('none','before','after','modified_after')),
  holiday_cal         TEXT,
  match_window_before INTEGER NOT NULL DEFAULT 1,
  match_window_after  INTEGER NOT NULL DEFAULT 4,
  match_amount_minor  INTEGER NOT NULL DEFAULT 0,
  match_amount_bp     INTEGER NOT NULL DEFAULT 0,
  match_auto          INTEGER NOT NULL DEFAULT 1 CHECK (match_auto IN (0,1)),
  scenario_id         INTEGER REFERENCES scenario(id) ON DELETE CASCADE,
  supersedes_id       INTEGER REFERENCES series(id),
  CHECK (dtstart = date(dtstart)),
  CHECK (until_on IS NULL OR until_on >= dtstart),
  CHECK (upper(rrule) NOT LIKE '%COUNT=%'),
  CHECK (upper(rrule) NOT LIKE '%FREQ=HOURLY%'
     AND upper(rrule) NOT LIKE '%FREQ=MINUTELY%'
     AND upper(rrule) NOT LIKE '%FREQ=SECONDLY%'),
  CHECK (supersedes_id IS NULL OR scenario_id IS NOT NULL)
) STRICT;
CREATE INDEX series_head     ON series(COALESCE(head_id, id));
CREATE INDEX series_scenario ON series(scenario_id);

-- The posting template as rows, not JSON: FK integrity on account_id, and
-- "which account does this series hit" becomes a query the matcher can use.
-- Exactly one 'primary' (the leg an override amount replaces, and the leg the
-- CSV matcher compares against) and one 'balancing' (absorbs the remainder).
CREATE TABLE series_posting (
  id           INTEGER PRIMARY KEY,
  series_id    INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  account_id   INTEGER NOT NULL,
  currency     TEXT NOT NULL,
  amount_minor INTEGER NOT NULL,
  role         TEXT NOT NULL DEFAULT 'other' CHECK (role IN ('primary','balancing','other')),
  FOREIGN KEY (account_id, currency) REFERENCES account(id, currency)
) STRICT;
CREATE INDEX series_posting_series ON series_posting(series_id);
CREATE UNIQUE INDEX series_posting_one_primary   ON series_posting(series_id) WHERE role = 'primary';
CREATE UNIQUE INDEX series_posting_one_balancing ON series_posting(series_id) WHERE role = 'balancing';

-- RFC 5545 RECURRENCE-ID (req 4). occurrence_on is the ORIGINAL slot date and
-- is immutable; moved_to is where the money actually moves.
--   action='amend' = modify this occurrence   (RECURRENCE-ID)
--   action='skip'  = EXDATE
--   action='add'   = RDATE: a slot that exists regardless of the rule. This is
--                    also how a real-money slot is pinned across a rule edit.
-- Precedence: 'add' and 'amend' are inclusions, 'skip' excludes -- EXCEPT that
-- a real txn claiming the slot outranks a skip (reality outranks intention).
CREATE TABLE series_override (
  series_id     INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  occurrence_on TEXT NOT NULL,
  action        TEXT NOT NULL CHECK (action IN ('amend','skip','add')),
  moved_to      TEXT,
  amount_minor  INTEGER,
  description   TEXT,
  note          TEXT,
  PRIMARY KEY (series_id, occurrence_on),
  CHECK (occurrence_on = date(occurrence_on)),
  CHECK (moved_to IS NULL OR moved_to = date(moved_to)),
  CHECK (action <> 'skip' OR (moved_to IS NULL AND amount_minor IS NULL)),
  CHECK (action <> 'amend' OR moved_to IS NOT NULL OR amount_minor IS NOT NULL
                           OR description IS NOT NULL OR note IS NOT NULL)
) STRICT, WITHOUT ROWID;

-- Populate from the GOV.UK bank-holidays JSON once a year. cal = 'GB-EAW' etc.
CREATE TABLE holiday (
  cal     TEXT NOT NULL,
  on_date TEXT NOT NULL,
  name    TEXT,
  PRIMARY KEY (cal, on_date)
) STRICT, WITHOUT ROWID;

-- ============ 5. INTEREST (req 9) ============
-- Split on the real axis, not on product type: interest_rule = HOW A BALANCE
-- ACCRUES; payment_rule = A PAYMENT WHOSE AMOUNT IS A FUNCTION OF A BALANCE.
-- All three shapes use both tables (savings has no payment_rule).
--   accrues_on: 'negative' = card/loan debt, 'positive' = savings.
--   capitalise_rrule: when accrued interest is posted. For a card this IS the
--     statement date. Reuses the same RFC 5545 machinery as series.
--   accrual_freq+periods_per_year replace a day_count column:
--     daily/365 = act/365f, daily/360 = act/360, per_period/12 = 30/360.
-- Interest always accrues and posts in the ACCOUNT'S OWN currency. The interest
-- engine performs no FX and holds no rate (enforced in Rust:
-- counter_account.currency must equal account.currency).
CREATE TABLE interest_rule (
  id                 INTEGER PRIMARY KEY,
  account_id         INTEGER NOT NULL REFERENCES account(id),
  counter_account_id INTEGER NOT NULL REFERENCES account(id),
  shape              TEXT NOT NULL CHECK (shape IN ('revolving','amortising','savings')),
  accrues_on         TEXT NOT NULL CHECK (accrues_on IN ('positive','negative')),
  accrual_freq       TEXT NOT NULL CHECK (accrual_freq IN ('daily','per_period')),
  capitalise_rrule   TEXT NOT NULL,
  capitalise_dtstart TEXT NOT NULL,
  grace_period       INTEGER NOT NULL DEFAULT 0 CHECK (grace_period IN (0,1)),
  rounding           TEXT NOT NULL DEFAULT 'half_away_from_zero'
                       CHECK (rounding IN ('half_away_from_zero','half_even','floor','ceil')),
  priority           INTEGER NOT NULL DEFAULT 100,
  scenario_id        INTEGER REFERENCES scenario(id) ON DELETE CASCADE,
  supersedes_id      INTEGER REFERENCES interest_rule(id),
  CHECK (grace_period = 0 OR shape = 'revolving'),
  CHECK (upper(capitalise_rrule) NOT LIKE '%COUNT=%'),
  CHECK (supersedes_id IS NULL OR scenario_id IS NOT NULL)
) STRICT;
CREATE INDEX interest_rule_account ON interest_rule(account_id);

-- Rates are TIME-SLICED, which is what models a 0% balance transfer expiring.
-- rate_basis: UK APR and AER are EFFECTIVE (compounding included); US APR and
-- UK savings 'gross' are NOMINAL. Getting this backwards is an 11% error on a
-- 24.9% card, not a rounding nit.
-- periodic_rate_e15 is DERIVED ONCE at write time (rust_decimal) and stored:
--   effective -> ((1+q)^(1/p)-1)*1e15    nominal -> q/p*1e15
-- so forecast() is pure integer arithmetic and bit-reproducible. Verified here:
-- 24.9% effective /365 -> 609345112730; 4.50% AER monthly on 5000.00 -> 522499
-- (as nominal it would be 522970, i.e. 4.71 wrong).
-- derivation_version: bump when the derivation changes, so a stale row is
-- detectable rather than silently altering an old projection.
CREATE TABLE interest_rate_period (
  id                 INTEGER PRIMARY KEY,
  rule_id            INTEGER NOT NULL REFERENCES interest_rule(id) ON DELETE CASCADE,
  effective_from     TEXT NOT NULL,
  effective_to       TEXT,
  quoted_rate_e15    INTEGER NOT NULL,
  rate_basis         TEXT NOT NULL CHECK (rate_basis IN ('effective','nominal')),
  periods_per_year   INTEGER NOT NULL CHECK (periods_per_year > 0),
  periodic_rate_e15  INTEGER NOT NULL,
  derivation_version INTEGER NOT NULL DEFAULT 1,
  UNIQUE (rule_id, effective_from),
  CHECK (effective_to IS NULL OR effective_to > effective_from)
) STRICT;

-- Card minimum, loan level payment and "pay GBP 300/month" are ONE mechanism.
-- level_payment_minor is derived at write time like periodic_rate_e15.
--   PMT = (PV - balloon*(1+i)^-n) * i / (1-(1+i)^-n)
-- Verified here: 100,000.00 @ 4% nominal x360 -> 47742 (matches GnuCash's own
-- documented answer exactly); 10,000.00 @ 7.9% effective x60 -> 20099/month,
-- final payment 20073, closing balance exactly 0 minor units.
-- allocation: across a card's child rate buckets. UK cards pay highest-rate
-- first; with a single (leaf) account allocate() returns one pair and this is
-- inert -- but the code path exists, so buckets are additive later.
CREATE TABLE payment_rule (
  id                  INTEGER PRIMARY KEY,
  account_id          INTEGER NOT NULL REFERENCES account(id),
  from_account_id     INTEGER NOT NULL REFERENCES account(id),
  interest_rule_id    INTEGER REFERENCES interest_rule(id),
  amount_kind         TEXT NOT NULL CHECK (amount_kind IN
                        ('fixed','pct_of_balance','pct_of_statement',
                         'interest_fees_plus_pct','full_statement','amortising_level')),
  fixed_minor         INTEGER,
  pct_e15             INTEGER,
  floor_minor         INTEGER,
  cap_minor           INTEGER,
  term_periods        INTEGER,
  balloon_minor       INTEGER NOT NULL DEFAULT 0,
  level_payment_minor INTEGER,
  rrule               TEXT NOT NULL,
  dtstart             TEXT NOT NULL,
  until_on            TEXT,
  due_offset_days     INTEGER,
  allocation          TEXT NOT NULL DEFAULT 'highest_rate_first'
                        CHECK (allocation IN ('highest_rate_first','lowest_rate_first','pro_rata')),
  priority            INTEGER NOT NULL DEFAULT 100,
  scenario_id         INTEGER REFERENCES scenario(id) ON DELETE CASCADE,
  supersedes_id       INTEGER REFERENCES payment_rule(id),
  CHECK (upper(rrule) NOT LIKE '%COUNT=%'),
  CHECK (supersedes_id IS NULL OR scenario_id IS NOT NULL),
  CHECK (amount_kind <> 'fixed' OR fixed_minor IS NOT NULL),
  CHECK (amount_kind NOT IN ('pct_of_balance','pct_of_statement','interest_fees_plus_pct')
         OR pct_e15 IS NOT NULL),
  CHECK (amount_kind <> 'amortising_level'
         OR (term_periods IS NOT NULL AND level_payment_minor IS NOT NULL
             AND interest_rule_id IS NOT NULL)),
  CHECK (amount_kind NOT IN ('pct_of_statement','full_statement','interest_fees_plus_pct')
         OR (interest_rule_id IS NOT NULL AND due_offset_days IS NOT NULL))
) STRICT;
CREATE INDEX payment_rule_account ON payment_rule(account_id);

-- REAL statements, not forecast state. This is how forecast() seeds its cycle
-- state at as_of: where in the cycle am I, and was the last statement cleared
-- (which decides the grace latch). Without it the FIRST projected cycle -- the
-- one the user checks -- is a guess.
CREATE TABLE statement (
  id                    INTEGER PRIMARY KEY,
  interest_rule_id      INTEGER NOT NULL REFERENCES interest_rule(id) ON DELETE CASCADE,
  statement_on          TEXT NOT NULL,
  due_on                TEXT NOT NULL,
  closing_balance_minor INTEGER NOT NULL,
  interest_minor        INTEGER NOT NULL DEFAULT 0,
  fees_minor            INTEGER NOT NULL DEFAULT 0,
  cleared_in_full       INTEGER NOT NULL DEFAULT 0 CHECK (cleared_in_full IN (0,1)),
  UNIQUE (interest_rule_id, statement_on)
) STRICT;

-- ============ 6. MULTI-CURRENCY ============
-- 1 base = num/den quote, in MAJOR units. EXACT RATIONAL, never a float and
-- never a fixed scale: Frankfurter's "1.1664" is stored as 11664/10000.
-- base_code is always book_meta.display_currency, so every lookup is one index
-- scan and a cross rate is one division. as_of is the date the SOURCE REPORTS,
-- never the date requested (BOE returns Friday's rate for a weekend query;
-- storing the requested date fabricates rates it never published).
-- Rate resolution is always "latest effective rate on or before d", which gives
-- history and forecast-flat-spot from ONE code path with no future branch.
-- Priority manual > boe > ecb is applied in the lookup ORDER BY, not a table.
CREATE TABLE fx_rate (
  base_code  TEXT NOT NULL REFERENCES currency(code),
  quote_code TEXT NOT NULL REFERENCES currency(code),
  as_of      TEXT NOT NULL,
  source     TEXT NOT NULL CHECK (source IN ('manual','boe','ecb')),
  num        INTEGER NOT NULL CHECK (num > 0),
  den        INTEGER NOT NULL CHECK (den > 0),
  fetched_at TEXT,
  PRIMARY KEY (base_code, quote_code, as_of, source),
  CHECK (base_code <> quote_code),
  CHECK (as_of GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]')
) STRICT, WITHOUT ROWID;

-- ============ 7. THE LEDGER (req 3, 6) ============
-- A txn balances PER CURRENCY, strictly, with no tolerance and no weight
-- function. Any txn touching >1 currency MUST carry conversion postings.
-- The three generator FKs are the deduplication key: a REAL imported rent
-- payment / interest charge / card payment claims the forecast slot it
-- discharges, and the forecast then stops projecting it. At most one may be
-- set. Rust also enforces: a txn may never point at a scenario-scoped rule.
-- fx_status='provisional' = converted at a stale fallback rate (BOE runs ~1
-- business day behind, verified today: asked 2026-08-28, got 2026-08-27).
CREATE TABLE txn (
  id               INTEGER PRIMARY KEY,
  occurred_on      TEXT NOT NULL,
  description      TEXT NOT NULL,
  payee            TEXT,
  source           TEXT NOT NULL CHECK (source IN ('manual','import')),
  series_id        INTEGER REFERENCES series(id),
  interest_rule_id INTEGER REFERENCES interest_rule(id),
  payment_rule_id  INTEGER REFERENCES payment_rule(id),
  occurrence_on    TEXT,
  fx_status        TEXT CHECK (fx_status IS NULL OR fx_status = 'provisional'),
  note             TEXT,
  CHECK (occurred_on = date(occurred_on)),
  CHECK (occurrence_on IS NULL OR occurrence_on = date(occurrence_on)),
  CHECK ((series_id IS NOT NULL) + (interest_rule_id IS NOT NULL)
       + (payment_rule_id IS NOT NULL) <= 1),
  CHECK (occurrence_on IS NOT NULL
         OR (series_id IS NULL AND interest_rule_id IS NULL AND payment_rule_id IS NULL))
) STRICT;
CREATE INDEX txn_date ON txn(occurred_on);
CREATE UNIQUE INDEX txn_series_occ   ON txn(series_id, occurrence_on)        WHERE series_id IS NOT NULL;
CREATE UNIQUE INDEX txn_interest_occ ON txn(interest_rule_id, occurrence_on) WHERE interest_rule_id IS NOT NULL;
CREATE UNIQUE INDEX txn_payment_occ  ON txn(payment_rule_id, occurrence_on)  WHERE payment_rule_id IS NOT NULL;

-- The composite FK is the point: posting.currency == account.currency is a
-- database guarantee, which kills a whole class of import bugs.
CREATE TABLE posting (
  id           INTEGER PRIMARY KEY,
  txn_id       INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  account_id   INTEGER NOT NULL,
  currency     TEXT NOT NULL,
  amount_minor INTEGER NOT NULL,
  FOREIGN KEY (account_id, currency) REFERENCES account(id, currency)
) STRICT;
CREATE INDEX posting_txn     ON posting(txn_id);
CREATE INDEX posting_account ON posting(account_id, currency);

CREATE TABLE txn_tag (
  txn_id INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  tag    TEXT NOT NULL,
  PRIMARY KEY (txn_id, tag)
) STRICT, WITHOUT ROWID;

-- req 10: a flow is ORDERED and has a terminus. seq gives the order, role
-- marks the arrival, and a txn may belong to more than one journey. This also
-- carries the multi-day FX transfer: GBP out Monday -> Assets:InTransit:GBP,
-- EUR in Wednesday with the conversion postings on the SECOND txn.
CREATE TABLE journey_member (
  journey_id INTEGER NOT NULL REFERENCES journey(id) ON DELETE CASCADE,
  txn_id     INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  seq        INTEGER NOT NULL,
  role       TEXT NOT NULL DEFAULT 'leg' CHECK (role IN ('source','leg','fee','arrival')),
  PRIMARY KEY (journey_id, txn_id)
) STRICT;
CREATE UNIQUE INDEX journey_member_seq ON journey_member(journey_id, seq);
CREATE INDEX journey_member_txn ON journey_member(txn_id);

-- ============ 8. CSV IMPORT ============
-- Columns are mapped BY HEADER NAME, keyed by header_fingerprint =
-- blake3(normalised header row + delimiter + has_header). PayPal alone kills
-- positional mapping: the user picks the column set at download time. A bank
-- changing its columns yields a NEW fingerprint -> wizard, not a silent
-- off-by-one. verified=0 seeds (HSBC, Wise) open the wizard pre-filled instead
-- of importing silently.
-- Format and landing are ONE table: two Starling accounts = two profiles that
-- share a fingerprint; the importer asks which when more than one matches.
CREATE TABLE import_profile (
  id                 INTEGER PRIMARY KEY,
  name               TEXT NOT NULL UNIQUE,
  verified           INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0,1)),
  verified_on        TEXT,
  header_fingerprint TEXT,
  delimiter          TEXT NOT NULL DEFAULT ',',
  quote_char         TEXT NOT NULL DEFAULT '"',
  encoding           TEXT NOT NULL DEFAULT 'auto'
                       CHECK (encoding IN ('auto','utf-8','windows-1252')),
  has_header         INTEGER NOT NULL DEFAULT 1 CHECK (has_header IN (0,1)),
  skip_leading       INTEGER NOT NULL DEFAULT 0,
  date_format        TEXT NOT NULL,
  decimal_sep        TEXT NOT NULL DEFAULT '.',
  thousands_sep      TEXT NOT NULL DEFAULT ',',
  mapping_json       TEXT NOT NULL,
  account_id         INTEGER REFERENCES account(id),
  account_by_column  TEXT,
  default_currency   TEXT NOT NULL REFERENCES currency(code),
  match_series       INTEGER NOT NULL DEFAULT 1 CHECK (match_series IN (0,1)),
  last_imported_on   TEXT
) STRICT;
CREATE INDEX import_profile_fp ON import_profile(header_fingerprint);

-- Batches and rows are PERSISTENT, not in-memory staging. That single choice
-- buys: postpone-a-row, whole-batch undo, re-run matching after adding a rule
-- without re-parsing, and provenance from a ledger line back to a CSV line.
-- It is also what lets a provisional match live somewhere without a separate
-- claim table on the ledger side.
CREATE TABLE import_batch (
  id               INTEGER PRIMARY KEY,
  profile_id       INTEGER NOT NULL REFERENCES import_profile(id),
  source_name      TEXT NOT NULL,
  file_fingerprint TEXT NOT NULL,
  imported_at      TEXT NOT NULL,
  row_count        INTEGER NOT NULL DEFAULT 0,
  first_row_on     TEXT,
  last_row_on      TEXT,
  state            TEXT NOT NULL DEFAULT 'staged'
                     CHECK (state IN ('staged','committed','reverted')),
  committed_at     TEXT
) STRICT;
CREATE INDEX import_batch_file ON import_batch(file_fingerprint);

-- Ordered, first-match-wins PER ACTION SLOT, so a specific rule can set the
-- account and a later generic rule can still add a tag. Conditions are real
-- columns, not JSON, so "why did this fire" is a query. hit_count is how dead
-- rules get found. Learning is an EXPLICIT rule offered in the review screen,
-- never a hidden classifier.
CREATE TABLE import_rule (
  id                 INTEGER PRIMARY KEY,
  name               TEXT NOT NULL,
  priority           INTEGER NOT NULL,
  enabled            INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  profile_id         INTEGER REFERENCES import_profile(id) ON DELETE CASCADE,
  field              TEXT NOT NULL CHECK (field IN ('description','payee','bank_category','txn_type')),
  op                 TEXT NOT NULL CHECK (op IN ('contains','equals','starts_with','regex')),
  pattern            TEXT NOT NULL,
  sign               INTEGER NOT NULL DEFAULT 0 CHECK (sign IN (-1,0,1)),
  set_far_account_id INTEGER REFERENCES account(id),
  set_payee          TEXT,
  add_tags           TEXT,
  hit_count          INTEGER NOT NULL DEFAULT 0,
  last_hit_on        TEXT
) STRICT;
CREATE INDEX import_rule_order ON import_rule(enabled, priority);

CREATE TABLE import_row (
  id              INTEGER PRIMARY KEY,
  batch_id        INTEGER NOT NULL REFERENCES import_batch(id) ON DELETE CASCADE,
  line_no         INTEGER NOT NULL,
  raw_json        TEXT NOT NULL,
  account_id      INTEGER REFERENCES account(id),
  occurred_on     TEXT,
  description     TEXT NOT NULL DEFAULT '',
  payee           TEXT,
  bank_category   TEXT,
  txn_type        TEXT,
  amount_minor    INTEGER,
  gross_minor     INTEGER,
  fee_minor       INTEGER NOT NULL DEFAULT 0,
  currency        TEXT REFERENCES currency(code),
  balance_minor   INTEGER,
  external_id     TEXT,
  fingerprint     TEXT,
  dup_ordinal     INTEGER NOT NULL DEFAULT 0,
  boundary_row    INTEGER NOT NULL DEFAULT 0 CHECK (boundary_row IN (0,1)),
  state           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending','new','duplicate','possible_duplicate',
                                     'matched','ambiguous','postponed','ignored','committed','error')),
  accepted        INTEGER NOT NULL DEFAULT 0 CHECK (accepted IN (0,1)),
  far_account_id  INTEGER REFERENCES account(id),
  rule_id         INTEGER REFERENCES import_rule(id) ON DELETE SET NULL,
  series_id       INTEGER REFERENCES series(id),
  occurrence_on   TEXT,
  match_score     INTEGER,
  candidates_json TEXT,
  dup_of_txn_id   INTEGER REFERENCES txn(id),
  pair_row_id     INTEGER REFERENCES import_row(id),
  merge_accepted  INTEGER NOT NULL DEFAULT 0 CHECK (merge_accepted IN (0,1)),
  txn_id          INTEGER REFERENCES txn(id) ON DELETE SET NULL,
  error           TEXT,
  note            TEXT,
  UNIQUE (batch_id, line_no)
) STRICT;
CREATE INDEX import_row_batch ON import_row(batch_id, state);
CREATE INDEX import_row_fp    ON import_row(account_id, fingerprint);

-- Dedup identity lives here, not on txn, because identity is per-ACCOUNT and
-- txn has no account. It also lets ONE txn carry TWO keys, which is exactly
-- what a merged transfer needs: a Starling->Wise move imported from both banks
-- becomes one transaction and re-importing either file still dedups.
CREATE TABLE txn_import_key (
  txn_id      INTEGER NOT NULL REFERENCES txn(id) ON DELETE CASCADE,
  account_id  INTEGER NOT NULL REFERENCES account(id),
  fingerprint TEXT NOT NULL,
  external_id TEXT,
  batch_id    INTEGER NOT NULL REFERENCES import_batch(id),
  line_no     INTEGER NOT NULL,
  PRIMARY KEY (txn_id, account_id, fingerprint)
) STRICT;
CREATE UNIQUE INDEX txn_import_fp  ON txn_import_key(account_id, fingerprint);
CREATE UNIQUE INDEX txn_import_ext ON txn_import_key(account_id, external_id) WHERE external_id IS NOT NULL;

-- ============ 9. INTEGRITY + EXPORT (req 7) ============
-- The first three views MUST return zero rows, exactly, with no epsilon.
-- That exactness is the whole payoff for balancing strictly per currency.
CREATE VIEW v_check_txn_unbalanced AS
SELECT txn_id, currency, SUM(amount_minor) AS residual_minor
FROM posting GROUP BY txn_id, currency HAVING SUM(amount_minor) <> 0;

CREATE VIEW v_check_book_unbalanced AS
SELECT currency, SUM(amount_minor) AS residual_minor
FROM posting GROUP BY currency HAVING SUM(amount_minor) <> 0;

CREATE VIEW v_check_missing_conversion AS
SELECT p.txn_id
FROM posting p JOIN account a ON a.id = p.account_id
GROUP BY p.txn_id
HAVING COUNT(DISTINCT p.currency) > 1
   AND SUM(CASE WHEN a.kind = 'conversion' THEN 1 ELSE 0 END) = 0;

-- FX gain/loss falls out of the conversion accounts with no lot tracking:
-- value this residual in GBP at date d. Zero at the rates that created it.
-- A POSITIVE residual is a LOSS. At today's rate it is average-cost
-- unrealised; once positions close it is realised and stops moving.
--   SELECT SUM(fx_convert(residual_minor, currency, 'GBP', :d))
--     FROM v_conversion_residual WHERE on_date <= :d;
-- NEVER net or garbage-collect conversion postings: they are the only record
-- of acquisition basis, and FIFO can only be added later if they survive.
CREATE VIEW v_conversion_residual AS
SELECT p.currency, t.occurred_on AS on_date, SUM(p.amount_minor) AS residual_minor
FROM posting p
JOIN account a ON a.id = p.account_id
JOIN txn     t ON t.id = p.txn_id
WHERE a.kind = 'conversion'
GROUP BY p.currency, t.occurred_on;

-- One row per POSTING; never fans out. The LLM export and the CSV export both
-- read this. amount_decimal is pre-formatted so the consumer never divides by
-- 100 (which would silently assume 2dp).
CREATE VIEW v_ledger_line AS
SELECT
  p.id AS posting_id, t.id AS txn_id, t.occurred_on AS on_date,
  a.name AS account, a.kind AS account_kind,
  t.description, t.payee,
  p.amount_minor, p.currency, c.minor_digits,
  CASE c.minor_digits
    WHEN 0 THEN printf('%d', p.amount_minor)
    WHEN 2 THEN printf('%s%d.%02d', CASE WHEN p.amount_minor<0 THEN '-' ELSE '' END,
                       abs(p.amount_minor)/100,  abs(p.amount_minor)%100)
    WHEN 3 THEN printf('%s%d.%03d', CASE WHEN p.amount_minor<0 THEN '-' ELSE '' END,
                       abs(p.amount_minor)/1000, abs(p.amount_minor)%1000)
    ELSE NULL END AS amount_decimal,
  t.series_id, s.description AS series_description, t.occurrence_on,
  t.interest_rule_id, t.payment_rule_id, t.fx_status, t.source,
  (SELECT group_concat(j.label, ' | ') FROM journey_member jm
     JOIN journey j ON j.id = jm.journey_id WHERE jm.txn_id = t.id) AS journeys,
  (SELECT group_concat(tag, ' ') FROM txn_tag WHERE txn_id = t.id)  AS tags,
  k.batch_id AS import_batch_id, k.line_no AS import_line_no,
  0 AS is_projection
FROM posting p
JOIN txn t     ON t.id   = p.txn_id
JOIN account a ON a.id   = p.account_id
JOIN currency c ON c.code = p.currency
LEFT JOIN series s ON s.id = t.series_id
LEFT JOIN txn_import_key k ON k.txn_id = t.id AND k.account_id = p.account_id;

CREATE VIEW v_export_facts AS
SELECT a.name AS account, a.kind AS account_kind, p.currency,
       COUNT(*) AS posting_count, MIN(t.occurred_on) AS first_on,
       MAX(t.occurred_on) AS last_on, SUM(p.amount_minor) AS closing_minor
FROM posting p JOIN txn t ON t.id = p.txn_id JOIN account a ON a.id = p.account_id
GROUP BY a.id, p.currency;

INSERT INTO currency(code, minor_digits, name) VALUES
  ('GBP',2,'Pound Sterling'), ('EUR',2,'Euro'), ('USD',2,'US Dollar'), ('JPY',0,'Yen');
INSERT INTO book_meta(key,value) VALUES
  ('schema_version','1'), ('display_currency','GBP'), ('rate_derivation_version','1');