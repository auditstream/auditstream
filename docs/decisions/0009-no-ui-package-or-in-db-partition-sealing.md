---
status: accepted
date: 2026-11-27
decision-makers: Gabriel Lobo, Cleisson
---

# Do not ship an official React UI package or in-database partition sealing

> **Amendment (2026-11-27; originally accepted 2026-11-24):** Preserves Options A1 and B1 in full while clarifying CloudTrail-style manifest-to-manifest SHA-256 chaining in [#112](https://github.com/auditstream/auditstream/issues/112), explicitly evaluating asynchronous `xid8`-window in-DB sealing (Option B2b), stating the cryptographic trust-boundary law and export-lag window (`Δt_export`), refining AWS QLDB wording, and recording live PostgreSQL 17.12 and 19.0 empirical benchmarks.

## Context and Problem Statement

Before cutting `v0.1.0`, we evaluated two features commonly seen in hosted and self-hosted audit log tools:

1. **An official `@auditstream/react` npm package** providing pre-built audit log table and diff viewer components.
2. **In-database cryptographic tamper-evidence**, either via per-row hot-path hash chaining, via monthly partition sealing over `auditstream.changes_YYYY_MM`, or via an asynchronous background job sealing committed `xid8` windows inside PostgreSQL.

Both proposals touch core architectural boundaries of Auditstream:

* [spec/actor.md](../../spec/actor.md) enforces an **IDs-only privacy rule**: actor payloads carry opaque identifiers (for example `user:usr_42`) rather than display names or email addresses, because SQL comments appear in `pg_stat_activity` and PostgreSQL logs.
* [spec/history-table.md](../../spec/history-table.md) defines `occurred_at` with `DEFAULT clock_timestamp()` (insert time, not commit time), isolates writes through the `auditstream_owner` role and `SECURITY DEFINER` functions with no direct write grants for application roles, and uses monthly partitions (`changes_YYYY_MM`) so expired data can be removed via `DROP TABLE`.
* [ADR-0006](0006-esm-only-typescript-packages-sharing-one-context.md) keeps `auditstream-js` focused on ESM-only, zero-dependency backend packages released together as a Changesets `fixed` group (`@auditstream/node`, `@auditstream/prisma`, `@auditstream/drizzle`).
* [ADR-0008](0008-history-lookup-index.md) protects write-side capture latency and avoids unnecessary background contention on the history table.

We need a clear, durable decision on whether either capability belongs in Auditstream's official packages or database engine, and what alternatives should serve users who need an audit log UI or compliance tamper-evidence.

## Considered Options

### For audit log UI

* **Option A1 (Reject official UI package):** Do not publish `@auditstream/react`. Provide a safe server-side query helper (`audit.query()`) in `@auditstream/node` and copy-paste Backend-for-Frontend (BFF) examples under `auditstream-js/examples/`.
* **Option A2 (Official `@auditstream/react` package):** Publish and maintain a versioned React component package inside `auditstream-js`.

### For tamper-evidence

* **Option B1 (Reject in-DB hash chaining and in-DB sealing; deliver external WORM + chained signed manifests in `#112`):** Keep PostgreSQL role separation (`auditstream_owner`) as the in-database write boundary. Deliver compliance tamper-evidence out-of-band through immutable export segments on WORM storage (such as Amazon S3 Object Lock Compliance Mode) paired with CloudTrail-style **manifest-to-manifest SHA-256 chaining** (`prev_manifest_sha256`, monotonic `chain_seq`, contiguous `(xact_id, id)` export window bounds, and segment SHA-256 digests), external asymmetric signing (Ed25519/ES256 or KMS/Vault), and an offline verification command ([#112](https://github.com/auditstream/auditstream/issues/112)).
* **Option B2a (Synchronous in-DB hot-path row hash chaining or calendar-month partition sealing):** Compute sequential per-row SHA-256 hashes inside capture triggers or seal closed `changes_YYYY_MM` partitions by calendar boundary inside PostgreSQL.
* **Option B2b (Asynchronous in-DB `xid8`-window sealing):** Run a background worker that periodically hashes committed `xid8` windows (`xact_id < pg_snapshot_xmin(pg_current_snapshot())`) and stores the resulting seals inside a PostgreSQL table.

## Decision Outcome

Chosen options: **Option A1 (no official `@auditstream/react` package)** and **Option B1 (no in-DB hot-path row hash chaining, calendar-month partition sealing, or in-DB async `xid8` sealing; deliver external WORM + chained signed manifests in `#112`)**.

### Why `@auditstream/react` is not built

* **Contract mismatch with IDs-only actors:** Generic UI components only see opaque IDs (`usr_42`) and raw `snake_case` columns. Every team still has to write server-side resolvers to map actor IDs to names and project safe fields.
* **Security boundary risk:** `auditstream.changes` rows contain internal database metadata (`db_user`, `xact_id`, `table_oid`, `actor_source`) and raw `before`/`after` payloads. Shipping a browser-oriented table encourages forwarding raw history rows to clients instead of projecting an allowlisted DTO on the server.
* **OLTP query pressure:** Generic table widgets commonly run `COUNT(*)` totals and unconstrained ad-hoc filters against the transactional PostgreSQL instance.
* **Disproportionate maintenance cost:** Maintaining an npm UI library couples a small infrastructure team to React major versions, React Server Components boundaries, styling churn (Tailwind CSS versions, Radix UI vs. Base UI), accessibility bugs, and frontend CVEs. Prior open-source audit UI packages (such as `@retracedhq/logs-viewer`) show near-zero adoption, while modern TypeScript infrastructure libraries increasingly rely on copy-paste examples or registries instead of versioned UI packages.
* **Approved alternative:** Expose safe, keyset-paginated querying (`audit.query()`) in `@auditstream/node` and maintain a copy-paste BFF Activity Feed example in `auditstream-js/examples/next-prisma` that strips internal fields and resolves actor labels on the server.

### Why in-DB row hash chaining and in-DB sealing (Options B2a and B2b) are not built

* **The cryptographic trust-boundary law and the export-lag window (`Δt_export`):** Tamper-evidence begins **only when a cryptographic digest crosses an independent trust boundary** (off-host WORM storage, an independent signing key/KMS, or an external witness). Application roles already hold no `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE` privileges on `auditstream.changes`. The only insider who can alter stored history is a database superuser or an administrator with `auditstream_owner` access — who can also rewrite rows `k..N` and recompute every downstream in-DB hash or seal in a single transaction, leaving **`0` broken links**.
  * **Honest accounting of `Δt_export`:** External manifest chaining has a finite export-lag window (`Δt_export`) between PostgreSQL commit and the next WORM manifest write. An in-DB hash chain has the **exact same unanchored gap (`Δt_anchor`)** against a rogue superuser or `auditstream_owner` administrator until its head digest leaves PostgreSQL.
  * **History mutation vs. capture bypass:** Distinguish post-hoc mutation of `auditstream.changes` (requiring `auditstream_owner` or superuser) from **capture bypass** at write time — via `ALTER TABLE ... DISABLE TRIGGER` (superuser or the audited table's owner) or `SET session_replication_role = replica` (superuser, or on PostgreSQL 15+ any role granted `SET ON PARAMETER session_replication_role`). Neither in-DB hashes nor external manifests alone can prove a write occurred if the trigger never fired. Mitigate both `Δt_export` and privileged bypass by running a short export cadence (`10–60 s`), shipping `pgAudit` (`pgaudit.log = 'ddl, role, misc_set'`) off-host to record `session_replication_role` and DDL/role changes, running `auditstream verify` continuously (which already flags disabled triggers as `disabled` and `session_replication_role` parameter grants or role/database `replica` overrides as `unsafe_privileges`), and adding a future opt-in DDL event trigger.
* **Hot-path write serialization and deadlocks (Option B2a):** Under PostgreSQL MVCC, concurrent transactions cannot observe each other's uncommitted rows, so a single linear per-row hash chain forces a global lock on the chain head (`SELECT ... FOR UPDATE`) held until `COMMIT`. That lock serializes all audited writes database-wide, violates [ADR-0008](0008-history-lookup-index.md), and induces `40P01` deadlocks even on mathematically deadlock-free lock-ordered multi-row transactions.
* **Race condition with `clock_timestamp()` (calendar-month sealing, Option B2a):** Because `occurred_at` defaults to `clock_timestamp()`, a transaction that starts before `00:00:00Z` on the first of the month and commits afterward makes new rows visible in the previous month's `changes_YYYY_MM` partition after the month boundary. Moving rows out of `changes_default` also mutates a past partition. Sealing monthly partitions by calendar boundary therefore produces false-positive tamper alerts.
* **Why asynchronous in-DB `xid8`-window sealing (Option B2b) is also rejected:** Sealing committed `xid8` windows (`xact_id < pg_snapshot_xmin`) avoids both hot-path lock contention and the `clock_timestamp()` month-boundary race. However, keeping those seals **inside PostgreSQL** fails three compliance requirements at once:
  1. **Same trust domain:** A rogue DBA or superuser can rewrite both `auditstream.changes` and the in-DB seal table unless the seal is exported to an independent trust boundary anyway.
  2. **Breaks on `DROP PARTITION`:** Dropping expired `changes_YYYY_MM` partitions for LGPD/GDPR storage limitation severs in-DB verification against historical rows.
  3. **No off-host archive or export-time redaction:** External WORM sealing over the exact same `xid8` windows (`#112`) directly supports **PCI-DSS v4.0.1 Req 10.3.3** off-host immutable archiving, survives monthly `DROP TABLE changes_YYYY_MM` retention drops cleanly, and preserves export-time redaction (`[[export.redaction.rules]]`) so personal data can be stripped before writing to immutable WORM archives (consistent with **EDPB Guidelines 02/2025**'s recommendation against storing personal data on immutable ledgers).
* **Collision with retention and privacy:** Monthly partitions exist so expired audit data can be removed cheaply via `DROP TABLE changes_YYYY_MM` ([Minimize PII and enforce retention under LGPD and GDPR](../how-to/lgpd-and-gdpr-compliance.md)). In-DB row hash chains spanning stored rows break on every routine retention drop and conflict with data-minimization workflows (EDPB Guidelines 02/2025).
* **Industry alignment:** Compliance frameworks (PCI-DSS v4.0.1 Req 10.3.2–10.3.4, SOC 2, ISO/IEC 27001:2022 A.8.15, NIST SP 800-92) prioritize write-once storage separation (WORM) and externally signed chained digests — exemplified by **AWS CloudTrail Log File Integrity Validation**. AWS ended Amazon QLDB support on **July 31, 2025** and directed ledger and audit workloads to **Amazon Aurora PostgreSQL** (`pgAudit`, history tables, and Database Activity Streams) paired with **Amazon S3 archival** (commonly hardened with S3 Object Lock). Even commercial database ledger features (Microsoft SQL Server Ledger's asynchronous digest generation paired with external digest anchoring — such as Azure Immutable Blob Storage, Azure Confidential Ledger, or another immutable store — and Oracle Blockchain Tables' sharded chains requiring external anchoring) avoid a global synchronous single-head lock on OLTP DML.
* **Approved alternative ([#112](https://github.com/auditstream/auditstream/issues/112)):** Rely on PostgreSQL role separation (`auditstream_owner`) inside the database, and deliver compliance tamper-evidence via an external WORM object-storage sink (such as S3 Object Lock Compliance Mode) writing immutable `xid8`-window export segments linked by **CloudTrail-style chained signed manifests** (`chain_seq`, `prev_manifest_sha256`, contiguous `(from_cursor, to_cursor)` `(xact_id, id)` bounds, segment SHA-256 digests, and external Ed25519/ES256 or KMS signatures) plus offline verification (`verify-export`).

#### Empirical PostgreSQL 17.12 and 19.0 evidence (synchronous row hash chaining vs. unchained Auditstream)

Measured on live PostgreSQL 17.12 and 19.0 (`fsync=on`, native built-in `pg_catalog.sha256`, lock-serialized `chain_head FOR UPDATE`):

| Workload (`fsync=on`) | Unchained Auditstream (PG 17.12 / PG 19.0) | In-Trigger Row Hash Chain (PG 17.12 / PG 19.0) | Observed impact |
| --- | --- | --- | --- |
| Single-row `UPDATE`, `c=128` | `24,576` / `25,060 TPS` (`p99 17.2` / `15.7 ms`) | `405` / `353 TPS` (`p99 1,615` / `1,715 ms`) | **`61×–71×` TPS collapse**, `94×–109×` worse `p99` (`98%–99%` `Lock:tuple` + `Lock:transactionid`) |
| Lock-ordered 2-row tx (`lo <= hi`), `c=64` | `8,483` / `8,280 TPS` (`0` failures) | `55.9` / `56.1 TPS` (`456` / `555` failed txns at `c=64`) | **`148×–152×` TPS collapse** + **`575`–`668` failed txns** across `c=16` & `c=64` runs (`456`/`555` at `c=64`) from `40P01` lock-order deadlocks on deadlock-free SQL |
| Multi-stmt tx with `2 ms` post-write work, `c=64` | `7,239` / `7,786 TPS` (`p99 15.2` / `15.8 ms`) | `85.3` / `99.5 TPS` (`p99 3,730` / `3,029 ms`) | **`78×–85×` TPS collapse**, `191×–245×` worse `p99` |
| 10,000-row bulk `INSERT` (median of 5 runs) | `45.4` / `43.4 ms` (set-based statement trigger) | `121.4` / `119.6 ms` (forced sequential loop) | **`2.7×–2.8×` slower** while globally locking all audited tables |

Live correctness and threat-model proofs reproduced on PostgreSQL 19.0:

* **Rogue `auditstream_owner` silent rewrite:** Mutating row `chain_seq = 50` and re-hashing rows `50..100` inside one transaction returned **`0` broken links** (`head_matches = true`), proving same-trust-domain hash chains do not stop a rogue DBA/owner.
* **`DROP PARTITION` vs. row hash chain:** Dropping an expired monthly partition (`DROP TABLE changes_2025_01`) immediately severed the in-DB hash chain (`broken_links: 0 -> 1`).
* **`clock_timestamp()` month-boundary race:** A transaction inserting just before `00:00:00Z` and committing after midnight altered the closed calendar-month partition digest (`1` row -> `2` rows, triggering a false tamper alert), whereas Auditstream's `xid8` snapshot window (`xact_id < pg_snapshot_xmin`) observed `1` committed row and cleanly deferred the late commit to the next window with `0` false alerts.

### Consequences

* Good, because `auditstream-js` stays zero-dependency, ESM-only, and strictly server-side under one Changesets `fixed` group.
* Good, because users get a realistic, BFF-safe Activity Feed starting point they own and style with their own UI stack, without Auditstream carrying React/CSS SemVer liability.
* Good, because write capture incurs zero hash-chain locking or deadlock overhead and monthly partition drops continue working without invalidating stored proofs.
* Good, because future tamper-evidence work ([#112](https://github.com/auditstream/auditstream/issues/112)) aligns with auditor-recognized WORM + CloudTrail-style chained signed manifests over `xid8` windows rather than same-trust-domain database hashes.
* Bad, because teams wanting a drop-in React admin widget must copy and adapt the example code instead of installing a single npm package.
* Bad, because teams requiring cryptographic tamper-evidence out of the box must wait for [#112](https://github.com/auditstream/auditstream/issues/112) or anchor exported files in their own WORM storage today, and must size their export cadence (`Δt_export`) and privileged-access logging (`pgAudit`) according to their threat model.

## More Information

* [Actor and Context Schema (IDs-only privacy rule)](../../spec/actor.md#privacy-rule-ids-only)
* [History Table and Functions Contract](../../spec/history-table.md)
* [ADR-0006: ESM-only TypeScript packages sharing one context](0006-esm-only-typescript-packages-sharing-one-context.md)
* [ADR-0008: History table lookup index shape](0008-history-lookup-index.md)
* [Minimize PII and enforce retention under LGPD and GDPR](../how-to/lgpd-and-gdpr-compliance.md)
* [Capture design](../handbook/capture-design.md)
* [Adapters (TypeScript)](../handbook/adapters.md)
* [Issue #112: WORM object-storage sink and chained signed export manifests](https://github.com/auditstream/auditstream/issues/112)
* [AWS CloudTrail Log File Integrity Validation](https://docs.aws.amazon.com/awscloudtrail/latest/userguide/cloudtrail-log-file-validation-intro.html)
* [Amazon S3 Object Lock (WORM)](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html)
* [AWS: Migrate an Amazon QLDB ledger to Amazon Aurora PostgreSQL](https://aws.amazon.com/blogs/database/migrate-an-amazon-qldb-ledger-to-amazon-aurora-postgresql/)
* [EDPB Guidelines 02/2025 on processing of personal data through blockchain technologies (v2.0)](https://www.edpb.europa.eu/our-work-tools/our-documents/guidelines/guidelines-022025-processing-personal-data-through_en)
* [PCI SSC Document Library (PCI-DSS v4.0.1)](https://www.pcisecuritystandards.org/document_library/)
* [NIST SP 800-92: Guide to Computer Security Log Management](https://csrc.nist.gov/pubs/sp/800/92/final)
* [WorkOS Audit Logs](https://workos.com/docs/audit-logs)
* [BoxyHQ / Retraced `logs-viewer`](https://github.com/retracedhq/logs-viewer)
