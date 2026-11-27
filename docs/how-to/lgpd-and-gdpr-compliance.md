# Minimize PII and Enforce Retention Under LGPD and GDPR

This guide provides a practical engineering blueprint for operating Auditstream under Brazil's **LGPD** (Lei nº 13.709/2018), the **EU GDPR** (Regulation (EU) 2016/679), and the **UK GDPR** on Auditstream `v0.1.x`, together with the `v0.2` and `v0.3` roadmap.

> **Not legal advice:** This document maps Auditstream's technical controls to statutory articles and regulatory guidance so engineers, DPOs, and auditors share an accurate baseline. Confirm retention periods, legal bases, and sector rules with your Data Protection Officer or legal counsel.

## Legal-basis map

Audit logs are not governed by consent-only erasure rules, nor are they exempt from data minimization. Every compliant deployment combines a valid legal basis with strict **minimization at capture** and **bounded retention**.

### LGPD (Lei nº 13.709/2018)

| Provision | Role for audit logs | Engineering implication |
| --- | --- | --- |
| **Art. 7º, II** | Compliance with a legal or regulatory obligation (where applicable: **Marco Civil da Internet, Lei nº 12.965/2014, Art. 15** — 6-month minimum application-access log retention for for-profit legal-entity internet application providers; **BACEN CMN Resolution 4.893/2021** and **BCB Resolution 85/2021**, as amended by CMN 5.274/2025 and BCB 538/2025, for BACEN-regulated financial and payment institutions) | Where a statutory or sector mandate applies, retaining security and access audit metadata for the required floor is legally mandatory, not optional. |
| **Art. 7º, VI** | Regular exercise of rights in judicial, administrative, or arbitral proceedings | Supports bounded audit retention for dispute, chargeback, and incident defense. |
| **Art. 7º, IX** | Legitimate interest (security, fraud prevention, and accountability) | Requires a balancing test and strict minimization of logged attributes; **cannot** justify logging sensitive personal data (*dados pessoais sensíveis*, governed by Art. 11). |
| **Art. 16, I** | Retention after termination of processing when needed to fulfill a legal or regulatory obligation | Retention must remain **bounded** to the applicable statutory or regulatory window, not indefinite. |
| **Art. 18, VI** | Right to elimination of personal data processed **with the data subject's consent** (Art. 7º, I) | Does **not** override statutory audit logs held under Art. 7º, II / Art. 16, I — do not conflate consent-based deletion with security log obligations. |
| **Art. 6º, III (*Necessidade*) & Art. 18, IV** | Minimization and right to anonymization, blocking, or elimination of **unnecessary or excessive** data | **The real LGPD liability for audit trails:** duplicating direct PII (`cpf`, `email`, `phone`, home address) into an append-only history table is excessive even when retaining security metadata is legally mandated. |

### EU/UK GDPR and European regulatory guidance

| Source | Guidance for audit logs | Engineering implication |
| --- | --- | --- |
| **GDPR Art. 6(1)(c) & (f)** | Lawful bases for audit logging: compliance with a legal obligation (**6(1)(c)**) or legitimate interests (**6(1)(f)**, subject to a balancing test and Art. 21 right to object) | Must always be paired with **Art. 5(1)(c)** (*data minimisation*) and **Art. 5(1)(e)** (*storage limitation*). |
| **GDPR Art. 17(3)(b) & (e)** | Erasure exemptions for compliance with a legal obligation under Union/Member State (or UK domestic) law and for establishment, exercise, or defense of legal claims | Narrow exemptions that cover minimized security metadata — not excessive copies of direct PII stored in audit rows. |
| **EDPB Guidelines 01/2025** (*Pseudonymisation*, Jan 2025) | Pseudonymous data remains personal data whenever remaining attributes or auxiliary information allow singling out an individual | Orphaned opaque IDs (such as `usr_42`) are **not automatically anonymous** after deleting a user profile — minimization must still be paired with **bounded retention**. |
| **EDPB Guidelines 02/2025** (*Blockchain technologies*, v2.0 adopted July 2026) | Recommends keeping direct personal data off immutable logs; warns that key destruction (**crypto-shredding**) is **not** an automatic, unconditional Art. 17 safe harbor | Keep direct PII out of `auditstream.changes` at capture time rather than relying on post-hoc log rewriting or key destruction. |
| **CNIL Délibération 2021-122** | Recommends a rolling **6 to 12 month** log retention ceiling unless sector law, litigation, or specific risk justifies a longer period | Matches monthly `auditstream.changes_YYYY_MM` partition expiration cleanly. |
| **UK ICO ("Putting data beyond use")** | Applies when immediate individual erasure is technically impossible (primarily **offline backups and cold/WORM archives**) under four cumulative safeguards: no operational use affecting the individual, no third-party access, strict security controls, and permanent deletion on rotation | Active `auditstream.changes` relies on minimization plus a lawful basis / erasure exemption; treat **PostgreSQL backups, WAL/PITR timelines, and segregated WORM archives** as beyond-use stores guarded against resurrecting deleted PII on restore until scheduled expiration. |

## Practical `v0.1.x` minimization playbook

Keep direct PII out of `auditstream.changes` before a single row is written.

### Avoid the `DELETE FROM users` DSAR trap

`auditstream_delete` runs `AFTER DELETE ... REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT` and stores `to_jsonb(r) - excluded_columns` in `auditstream.changes.before`.
If an audited `public.users` table stores unexcluded `email`, `cpf`, `full_name`, or `phone` columns, executing a DSAR deletion (`DELETE FROM public.users WHERE id = 42;`) **copies the user's full PII into `auditstream.changes.before`**.

Prevent this using Pattern A (strongest in `v0.1.x`) or Pattern B.

### Pattern A (recommended in `v0.1.x`): split direct PII into an unaudited table

Store operational state in an audited table with a surrogate primary key, and store direct personal data in a sibling `1:1` table that is **not** in `capture.tables`:

```sql
-- Audited operational state (opaque surrogate PK, no direct PII)
CREATE TABLE public.users (
  id uuid PRIMARY KEY,
  status text NOT NULL,
  role text NOT NULL,
  plan text NOT NULL
);

-- Unaudited direct PII table (omitted from capture.tables)
CREATE TABLE public.user_pii (
  user_id uuid PRIMARY KEY REFERENCES public.users (id) ON DELETE CASCADE,
  full_name text NOT NULL,
  email text NOT NULL,
  cpf text,
  phone text
);
```

```toml
# auditstream.toml
[capture]
tables = ["public.users", "public.invoices"]
```

Why Pattern A is fail-closed in `v0.1.x`:

- Adding a new PII column to `public.user_pii` (for example `birth_date`) can never leak into `auditstream.changes`.
- Fulfilling a DSAR deletion by running `DELETE FROM public.user_pii WHERE user_id = $1;` writes **zero** rows to `auditstream.changes` (because `public.user_pii` is unaudited); running `DELETE FROM public.users WHERE id = $1;` (with `ON DELETE CASCADE` to `user_pii`) records only `{id, status, role, plan}` in `changes.before` — zero PII enters the history table.

### Pattern B: exclude PII columns with `capture.exclude`

When splitting tables is not immediately possible, list **every** PII and secret column in `capture.exclude` so triggers strip them before writing `before`, `after`, and `changed`:

```toml
# auditstream.toml
[capture]
tables = ["public.users"]
exclude = [
  "public.users.full_name",
  "public.users.email",
  "public.users.cpf",
  "public.users.phone",
  "public.users.password_hash",
]
```

Operational caveats for Pattern B on `v0.1.x`:

- **`ALTER TABLE ... ADD COLUMN` is fail-open until `v0.2`:** `capture.exclude` is a denylist. Adding a new PII column (for example `ALTER TABLE public.users ADD COLUMN national_id text;`) begins capturing that column immediately until you add it to `capture.exclude` and re-apply triggers. Enforce schema review in CI until `v0.2` ships opt-in per-table `capture.include` allowlists ([#172](https://github.com/auditstream/auditstream/issues/172)).
- **`RENAME COLUMN` and `DROP COLUMN` drift detection works today:** Run [`auditstream verify`](../reference/cli/verify.md) in CI after every migration. `verify` fails with `invalid_exclusion` (`error`) whenever a column in `capture.exclude` or on an installed trigger is renamed or dropped.
- **Always use surrogate primary keys:** Primary key columns cannot be excluded (`capture.exclude` rejects PK columns) and `[[export.redaction.rules]]` never modifies `record_id`. Never use `email`, `cpf`, `ssn`, or `phone` as the primary key of an audited table or inside `auditstream.log_event()` `target` payloads. See [Configuration reference: Export redaction](../reference/config.md#export-redaction-exportredactionrules).

### Actor, context, and export-time redaction hygiene

- **Actor payloads are IDs-only ([`spec/actor.md`](../../spec/actor.md#privacy-rule-ids-only)):** Send opaque identifiers only (`{"type":"user","id":"usr_42"}`), never display names or emails. Trailing SQL comments appear in `pg_stat_activity`, PostgreSQL logs, and `pg_stat_statements`.
- **`context.ip` and `context.userAgent` are opt-in only:** Leave both omitted unless explicitly required by your security or statutory logging mandate — for example Marco Civil Art. 15 application-access IP metadata (Art. 15 covers date, time, and IP address, not `userAgent`) — and covered by your retention schedule.
- **Export-time redaction (`[[export.redaction.rules]]`):** Use `mode = "remove"` or `mode = "mask"` to minimize `auditstream.log_event()` `data` payloads or produce reduced SIEM/customer feeds without altering PostgreSQL history. Never treat export-time redaction as a substitute for `capture.exclude` or Pattern A on prohibited PII or secrets.

## Bounded retention and monthly partition disposal

`auditstream.changes` is range-partitioned by UTC month (`auditstream.changes_YYYY_MM`), so expiring old audit history is an `O(1)` metadata drop with zero `DELETE` bloat or vacuum storms.

Size your retention window between your applicable mandatory/contractual **floors** (for example 6 completed prior months under Marco Civil Art. 15 where applicable, or 12 months total — with at least the most recent 3 months immediately available for analysis and months 4–12 optionally in off-host WORM storage — under PCI-DSS v4.0.1 Req 10.5.1) and applicable storage-limitation **ceilings** (for example CNIL Délibération 2021-122's rolling 6–12 month recommendation unless sector law or specific risk justifies longer). Always retain the **current UTC month plus `N` full prior UTC months** so retention floors are never clipped mid-month.

### Pre-drop checklist (`v0.1.x`)

Before detaching and dropping a monthly partition (for example `auditstream.changes_2025_01`):

1. **Confirm every active stateful export cursor and boundary `anchor` has moved past the partition:** Inspect **every** active stateful `auditstream export` cursor file — both unfiltered archive cursors (which verify full-table export completeness) and any filtered (`--source`, `--tenant`, `--actor`, `--since`, `--until`) cursors — and record the **lowest** `xact_id` across all `cursor.xact_id` and `anchor.xact_id` values as your `cutoff_xact_id`. Dropping a partition that still holds *any* stateful cursor's saved `anchor` row causes its next `auditstream export` resume to fail closed with exit code `3`. See [`auditstream export`: Checkpoints and delivery](../reference/cli/export.md#checkpoints-and-delivery).
2. **Confirm `changes_default` holds no expired rows:** Run `auditstream verify` as a role with unobstructed `SELECT` visibility on `auditstream.changes` (such as `auditstream_verify` or a superuser) — or pass `--fail-on warning` when Row-Level Security is enabled. When RLS hides `changes_default` from the verifying role, `verify` emits `rls_hides_default_partition` (`warning`) and exits `0` under default `--fail-on error`; with unobstructed visibility, any row in `auditstream.changes_default` fails with `default_partition_rows` (exit `1`).
3. **Never use `DETACH PARTITION ... CONCURRENTLY`:** PostgreSQL forbids `DETACH CONCURRENTLY` whenever a `DEFAULT` partition (`changes_default`) is attached, and `CONCURRENTLY` cannot run inside a transaction (which would risk leaving a temporary `auditstream_owner` role grant active on managed PostgreSQL).

### Manual 2-transaction partition disposal (`v0.1.x`)

First run a read-only pre-check **outside any transaction** (as an unobstructed role such as `auditstream_exporter`, `auditstream_verify`, or a superuser) querying parent `auditstream.changes` over the partition's exact UTC month bounds (`AccessShareLock` only, no `ACCESS EXCLUSIVE` lock on `auditstream.changes`, and no direct child-partition privileges needed) to confirm `max_xact_id < cutoff_xact_id`:

```sql
-- Read-only pre-check (AccessShareLock only; works as auditstream_exporter, auditstream_verify,
-- or superuser without needing child-partition grants): confirm max_xact_id is strictly below your
-- lowest active cursor.xact_id and anchor.xact_id (0 rows means the month has no rows).
-- Uses exact xid8 ordering via changes_xact_id_idx (never MAX(xact_id::text), which sorts lexicographically).
SELECT xact_id::text AS max_xact_id, id AS max_id
  FROM auditstream.changes
 WHERE occurred_at >= '2025-01-01T00:00:00Z'::timestamptz
   AND occurred_at <  '2025-02-01T00:00:00Z'::timestamptz
 ORDER BY xact_id DESC, id DESC
 LIMIT 1;
```

Next, detach and rename inside a short-timeout transaction (**TX1**) that verifies the `cutoff_xact_id` invariant non-interactively in milliseconds (aborting automatically without holding `ACCESS EXCLUSIVE` for human review), then drop the detached table outside parent locks (**TX2**).

Substitute your migration role for `migrator_role`, the expired month for `changes_2025_01`, and your lowest active cursor/anchor `xact_id` for `100000`. If you installed Auditstream with a custom `--schema`, replace the `auditstream.` schema prefix (keep the `auditstream_owner` role name literal). On **PostgreSQL 15**, omit `WITH INHERIT FALSE, SET TRUE`; as a **superuser**, omit the temporary `GRANT`/`REVOKE` lines and run `SET LOCAL ROLE auditstream_owner;` directly. Do not run the `GRANT`/`REVOKE` elevation snippets inside a session that already has a different active `SET ROLE` unless you first `RESET ROLE` or pass `GRANTED BY`.

```sql
-- TX1: Detach the expired partition under a bounded lock timeout, fail closed if any row
--      is at or above your lowest active cursor/anchor xact_id, and rename to __pruning.
BEGIN;
SET LOCAL lock_timeout = '5s';

-- Managed PostgreSQL 16+ migrator with ADMIN OPTION on auditstream_owner:
GRANT auditstream_owner TO migrator_role WITH INHERIT FALSE, SET TRUE;
SET LOCAL ROLE auditstream_owner;

ALTER TABLE auditstream.changes
  DETACH PARTITION auditstream.changes_2025_01;

DO $$
DECLARE
  v_cutoff xid8 := '100000'::xid8; -- lowest cursor.xact_id / anchor.xact_id across all active stateful exports
  v_max_xact_id xid8;
  v_max_id bigint;
BEGIN
  SELECT xact_id, id
    INTO v_max_xact_id, v_max_id
    FROM auditstream.changes_2025_01
   ORDER BY xact_id DESC, id DESC
   LIMIT 1;

  IF FOUND AND v_max_xact_id >= v_cutoff THEN
    RAISE EXCEPTION 'Refusing to prune auditstream.changes_2025_01: max (xact_id=%, id=%) >= cutoff %',
      v_max_xact_id, v_max_id, v_cutoff;
  END IF;
END $$;

ALTER TABLE auditstream.changes_2025_01
  RENAME TO changes_2025_01__pruning;

SET LOCAL ROLE NONE;
REVOKE auditstream_owner FROM migrator_role;
COMMIT;

-- TX2: Drop the detached table after releasing all locks on parent auditstream.changes.
BEGIN;
SET LOCAL lock_timeout = '5s';

GRANT auditstream_owner TO migrator_role WITH INHERIT FALSE, SET TRUE;
SET LOCAL ROLE auditstream_owner;

DROP TABLE IF EXISTS auditstream.changes_2025_01__pruning;

SET LOCAL ROLE NONE;
REVOKE auditstream_owner FROM migrator_role;
COMMIT;
```

Run `auditstream verify` after TX2 to confirm `ok: no findings` (including zero leftover `auditstream_owner` memberships).

> **Automated retention in `v0.2`:** Declarative `[retention]` configuration and commit-safe `auditstream prune` — automating the 2-transaction `DETACH` + `__pruning` rename + `DROP` flow with fail-closed cursor, filter, and `changes_default` guards — are tracked in [#145](https://github.com/auditstream/auditstream/issues/145).

## Why `auditstream erase` and in-trigger crypto-shredding are non-goals

### Why Auditstream does not ship an `auditstream erase` CLI command

- **Breaks compliance immutability:** PCI-DSS v4.0.1 Req 10.3.2, SOC 2, and ISO/IEC 27001:2022 A.8.15 require audit logs to be protected from routine alteration or deletion. Shipping an official CLI tool that mutates `auditstream.changes` undermines the append-only guarantee auditors evaluate.
- **Gives a false DSAR erasure guarantee:** Running row-level `DELETE` or `UPDATE` on `auditstream.changes` does not erase plaintext from PostgreSQL MVCC dead tuples (until `VACUUM` / page reuse), WAL archives and PITR timelines, database snapshots and backups, external SIEMs, or WORM object-storage exports ([#112](https://github.com/auditstream/auditstream/issues/112)).
- **No universal subject index and cursor fragility:** Per [ADR-0008](../decisions/0008-history-lookup-index.md), `auditstream.changes` intentionally indexes `record_id` rather than arbitrary cross-table subject graphs, and deleting rows can invalidate an active export cursor's saved `anchor` row (exiting `3` on the next export).

### Why in-trigger crypto-shredding is not built

- Generic database triggers do not know cross-table data-subject ownership graphs, cannot preserve Auditstream's `+5.1 µs/row` capture budget or JSONB `changed[]` diffing over per-subject ciphertexts, and would store keys inside the same database backup trust domain unless coupled to external network KMS calls inside every transaction.
- **EDPB Guidelines 02/2025** explicitly warn that destroying encryption keys is **not** an automatic, unconditional GDPR Art. 17 erasure safe harbor.

### Pattern that works today: application-layer envelope encryption

If your architecture already uses **application-layer envelope encryption** (for example AWS KMS or HashiCorp Vault Transit) to store ciphertext tokens (such as `vault:v1:...`) in PostgreSQL columns before executing SQL writes, Auditstream works transparently without trigger changes:

- Capture triggers record only opaque ciphertext in `before` and `after`.
- `changed[]` still detects modifications to encrypted columns.
- Encryption keys stay strictly outside PostgreSQL and its backups.

Treat application-layer envelope encryption as **defense-in-depth** alongside [minimization](#practical-v01x-minimization-playbook) and [bounded partition retention](#bounded-retention-and-monthly-partition-disposal) — not as a standalone replacement for keeping direct PII off immutable logs (EDPB Guidelines 02/2025).

## Emergency break-glass DBA runbook (accidental PII misconfiguration only)

> **Incident-response use only — NEVER for routine DSAR requests.** Use this runbook strictly when a misconfiguration (for example an unexcluded `cpf` or secret column added by a migration) accidentally wrote prohibited PII or credentials into `auditstream.changes`.

### Step 1: Stop the leak before scrubbing

1. Add the column to `capture.exclude` (or move it to an unaudited table per Pattern A), regenerate and apply the migration, and confirm `auditstream verify` passes cleanly.

### Step 2: Preview affected rows (read-only)

Run this preview **only as a role with unobstructed `SELECT` visibility** on `auditstream.changes` (`auditstream_exporter` or `auditstream_verify` when RLS is enabled, a superuser, or temporarily elevated `auditstream_owner`) — **never** as `app_user` or a `NOBYPASSRLS` role without an unfiltered `SELECT` policy, which can silently return `affected_rows = 0`. Use constant RFC 3339 timestamps covering the misconfiguration window so PostgreSQL prunes untouched monthly partitions:

```sql
SELECT COUNT(*) AS affected_rows,
       MIN(occurred_at) AS oldest_row,
       MAX(occurred_at) AS newest_row
  FROM auditstream.changes
 WHERE source = 'db'
   AND schema_name = 'public'
   AND table_name = 'users'
   AND occurred_at >= '2026-11-01T00:00:00Z'::timestamptz
   AND occurred_at <  '2026-12-01T00:00:00Z'::timestamptz
   AND ((before ? 'cpf') OR (after ? 'cpf') OR ('cpf' = ANY(COALESCE(changed, '{}'::text[]))));
```

### Step 3: Scrub only the leaked JSONB key and record a tombstone

Scrubbing the leaked key in place (`before - 'cpf'`, `after - 'cpf'`) instead of deleting the row preserves `id`, `event_id`, `xact_id`, and `auditstream export` cursor `anchor` continuity. Record a compensating `auditstream.log_event('compliance.remediation_executed', ...)` tombstone inside the same transaction using **opaque incident metadata only** — never include the scrubbed PII value or subject PII in `target` or `data`. If the leak spanned multiple UTC months, repeat Steps 3–4 once per affected `changes_YYYY_MM` partition.

Substitute your migration role for `migrator_role`, the target monthly partition for `changes_2026_11`, the leaked column for `cpf`, the operator ID for `dba_ops_1`, and your incident ticket for `INC-2026-0142`. If you use a custom `--schema`, replace only the `auditstream.` schema prefix — keep `auditstream_owner` and `'auditstream.actor'` literal. On **PostgreSQL 15**, omit `WITH INHERIT FALSE, SET TRUE`; as a **superuser**, omit the `GRANT`/`REVOKE` statements.

```sql
BEGIN;
SET LOCAL lock_timeout = '5s';

-- Managed PostgreSQL 16+ migrator with ADMIN OPTION on auditstream_owner:
GRANT auditstream_owner TO migrator_role WITH INHERIT FALSE, SET TRUE;
SET LOCAL ROLE auditstream_owner;

-- 1. Attribute the remediation operator for the compensating tombstone event.
SELECT pg_catalog.set_config(
  'auditstream.actor',
  '{"type":"user","id":"dba_ops_1","context":{"requestId":"INC-2026-0142","source":"cli"}}',
  true
);

-- 2. Strip ONLY the accidentally captured top-level JSONB key(s) in the affected partition
--    (leaving id, event_id, xact_id, occurred_at, and unrelated audit fields intact) and log a
--    PII-free compensating tombstone event in the same transaction when rows are scrubbed.
--    Returns 0 rows (and writes no tombstone) if the filter matched nothing (for example a typo).
WITH scrubbed AS (
  UPDATE auditstream.changes_2026_11
     SET before  = CASE WHEN before  IS NOT NULL THEN before  - 'cpf' ELSE NULL END,
         after   = CASE WHEN after   IS NOT NULL THEN after   - 'cpf' ELSE NULL END,
         changed = CASE WHEN changed IS NOT NULL THEN pg_catalog.array_remove(changed, 'cpf') ELSE NULL END
   WHERE source = 'db'
     AND schema_name = 'public'
     AND table_name = 'users'
     AND ((before ? 'cpf') OR (after ? 'cpf') OR ('cpf' = ANY(COALESCE(changed, '{}'::text[]))))
  RETURNING id
),
summary AS (
  SELECT COUNT(*) AS rows_scrubbed FROM scrubbed
)
SELECT summary.rows_scrubbed,
       auditstream.log_event(
         'compliance.remediation_executed',
         pg_catalog.jsonb_build_object('ticket', 'INC-2026-0142', 'partition', 'auditstream.changes_2026_11'),
         pg_catalog.jsonb_build_object(
           'reason', 'accidental_pii_capture',
           'table', 'public.users',
           'scrubbed_column', 'cpf',
           'rows_scrubbed', summary.rows_scrubbed
         )
       ) AS remediation_event_id
  FROM summary
 WHERE summary.rows_scrubbed > 0;

SET LOCAL ROLE NONE;
REVOKE auditstream_owner FROM migrator_role;
COMMIT;
```

### Step 4: Reclaim dead tuples and verify catalog hygiene

`auditstream.changes_2026_11` is owned by `auditstream_owner` (`NOLOGIN`), and `auditstream verify` forbids granting `MAINTAIN` to non-owners. Running bare `VACUUM` as a role that is neither `auditstream_owner`, the database owner, a superuser, nor (on PostgreSQL 17+) a member of `pg_maintain` **does not error** — PostgreSQL emits only a `WARNING` (`skipping ...` / `permission denied to vacuum ...`) and leaves scrubbed dead tuples containing leaked PII on disk.

Because `VACUUM` cannot run inside a transaction block, execute it on a **direct (non-pooled) connection** as a superuser, as the database owner or an inheriting (`INHERIT`) PG 17+ `pg_maintain` member that **also holds `USAGE` on schema `auditstream`**, or briefly elevated to `auditstream_owner`, and confirm the `VERBOSE` output shows zero `skipping` warnings:

```sql
-- Direct (non-pooled) connection only; outside a transaction block.
-- Omit GRANT/SET ROLE/RESET ROLE/REVOKE only if running as superuser, or as the database owner /
-- inheriting (INHERIT) PG 17+ pg_maintain member that also holds USAGE on schema auditstream
-- (otherwise keep this elevation block, since schema lookup checks USAGE for current_user before
-- evaluating VACUUM privileges).
-- On PostgreSQL 15, use: GRANT auditstream_owner TO migrator_role;
GRANT auditstream_owner TO migrator_role WITH INHERIT FALSE, SET TRUE;
SET ROLE auditstream_owner;

-- Marks dead tuple space reusable without taking an ACCESS EXCLUSIVE lock:
VACUUM (VERBOSE) auditstream.changes_2026_11;

-- Or, if your incident policy requires rewriting physical heap pages immediately rather than
-- waiting for page reuse, run during a maintenance window (takes ACCESS EXCLUSIVE on the partition):
-- VACUUM (FULL, VERBOSE) auditstream.changes_2026_11;

RESET ROLE;
REVOKE auditstream_owner FROM migrator_role;
```

> **Failure recovery & MVCC horizon:** Because `VACUUM` runs outside a transaction, if your session errors or disconnects before `REVOKE`, reconnect and run `RESET ROLE; REVOKE auditstream_owner FROM migrator_role;` so temporary `SET` membership is not left behind (which fails `auditstream verify` with `unsafe_privileges`). Also note that any long-running transaction, prepared transaction, or lagging replication slot holding PostgreSQL's `oldest_xmin` behind Step 3's commit prevents `VACUUM` from reclaiming those dead tuples until it finishes — re-run `VACUUM` after the horizon advances.

Run `auditstream verify` and confirm `ok: no findings`.

### Step 5: Document remaining archive and downstream boundaries

Record these boundaries honestly in your incident ticket and DPO notification:

- **PostgreSQL WAL, PITR, and backups:** Prior WAL segments and base backups retain the pre-scrub bytes until their backup retention window expires.
- **External SIEMs and WORM archives:** Any events already exported before the scrub (including future S3 Object Lock Compliance Mode archives from [#112](https://github.com/auditstream/auditstream/issues/112)) cannot be edited in place; document the affected export window and WORM/SIEM expiration date, or run a fresh redacted replay (`auditstream export --from-start` with `[[export.redaction.rules]]`) for mutable downstream consumers.

## Capabilities today vs. planned roadmap

| Capability | `v0.1.x` (available today) | Planned roadmap |
| --- | --- | --- |
| Keep PII out of `auditstream.changes` | Pattern A (`user_pii` table split) or Pattern B (`capture.exclude`) + IDs-only actors | **`v0.2`** ([#172](https://github.com/auditstream/auditstream/issues/172)): Opt-in per-table `capture.include` allowlist (fail-closed on `ADD COLUMN`) |
| Detect renamed/dropped excluded columns | `auditstream verify` (`invalid_exclusion`, `error`) | **`v0.2`** ([#172](https://github.com/auditstream/auditstream/issues/172), [#167](https://github.com/auditstream/auditstream/issues/167)): Extended to `capture.include` and `tenant_column` |
| Minimize outbound exports / app events | `[[export.redaction.rules]]` (`mask` / `remove`) | **`v0.3`** ([#112](https://github.com/auditstream/auditstream/issues/112)): Preserved and recorded in signed export manifests |
| Bounded monthly partition retention | Manual 2-TX `DETACH` + `DROP TABLE` ([above](#bounded-retention-and-monthly-partition-disposal)) | **`v0.2`** ([#145](https://github.com/auditstream/auditstream/issues/145)): Declarative `[retention]` + commit-safe `auditstream prune` |
| Cryptographic tamper-evidence | PostgreSQL role separation (`auditstream_owner`) + `auditstream verify` + self-managed WORM archival | **`v0.3`** ([#112](https://github.com/auditstream/auditstream/issues/112)): S3/WORM sink + CloudTrail-style chained signed manifests (`prev_manifest_sha256`) + `verify-export` |

## Related documentation

- [ADR-0009: Do not ship an official React UI package or in-database partition sealing](../decisions/0009-no-ui-package-or-in-db-partition-sealing.md)
- [Actor and Context Schema (IDs-only privacy rule)](../../spec/actor.md#privacy-rule-ids-only)
- [Configuration reference (`capture.exclude` and `export.redaction`)](../reference/config.md)
- [History table and `log_event` reference](../reference/history-table.md)
- [Isolate tenants with Row-Level Security (RLS)](multi-tenant-rls.md)
- [Export incrementally to JSON Lines or CSV](export-incrementally.md)
- [Check triggers in CI with `verify`](verify-in-ci.md)
