---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# Do not ship an official React UI package or in-database partition sealing

## Context and Problem Statement

Before cutting `v0.1.0`, we evaluated two features commonly seen in hosted and self-hosted audit log tools:

1. **An official `@auditstream/react` npm package** providing pre-built audit log table and diff viewer components.
2. **In-database cryptographic tamper-evidence**, either via per-row hot-path hash chaining or via monthly partition sealing over `auditstream.changes_YYYY_MM`.

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

* **Option B1 (Reject in-DB hash chaining and partition sealing):** Keep PostgreSQL role separation (`auditstream_owner`) as the in-database write boundary. Deliver compliance tamper-evidence out-of-band in a future release through immutable export files on WORM storage (such as S3 Object Lock Compliance Mode) paired with externally signed SHA-256 export manifests and an offline verification command.
* **Option B2 (In-DB hot-path hash chain or monthly partition sealing):** Compute row hashes inside capture triggers or seal closed `changes_YYYY_MM` partitions inside PostgreSQL.

## Decision Outcome

Chosen options: **Option A1 (no official `@auditstream/react` package)** and **Option B1 (no hot-path hash chaining or in-database partition sealing)**.

### Why `@auditstream/react` is not built

* **Contract mismatch with IDs-only actors:** Generic UI components only see opaque IDs (`usr_42`) and raw `snake_case` columns. Every team still has to write server-side resolvers to map actor IDs to names and project safe fields.
* **Security boundary risk:** `auditstream.changes` rows contain internal database metadata (`db_user`, `xact_id`, `table_oid`, `actor_source`) and raw `before`/`after` payloads. Shipping a browser-oriented table encourages forwarding raw history rows to clients instead of projecting an allowlisted DTO on the server.
* **OLTP query pressure:** Generic table widgets commonly run `COUNT(*)` totals and unconstrained ad-hoc filters against the transactional PostgreSQL instance.
* **Disproportionate maintenance cost:** Maintaining an npm UI library couples a small infrastructure team to React major versions, React Server Components boundaries, styling churn (Tailwind CSS versions, Radix UI vs. Base UI), accessibility bugs, and frontend CVEs. Prior open-source audit UI packages (such as `@retracedhq/logs-viewer`) show near-zero adoption, while modern TypeScript infrastructure libraries increasingly rely on copy-paste examples or registries instead of versioned UI packages.
* **Approved alternative:** Expose safe, keyset-paginated querying (`audit.query()`) in `@auditstream/node` and maintain a copy-paste BFF Activity Feed example in `auditstream-js/examples/next-prisma` that strips internal fields and resolves actor labels on the server.

### Why hot-path hash chaining and in-DB partition sealing are not built

* **Threat-model mismatch inside the database:** Application roles already hold no `INSERT`, `UPDATE`, `DELETE`, or `TRUNCATE` privileges on `auditstream.changes`. The only actor who can alter a historical partition is a database superuser or `auditstream_owner` administrator, who can also disable triggers (`session_replication_role = replica`) and recompute any unanchored hash or seal stored in the same database.
* **Hot-path write serialization:** Per-row hash chaining forces sequential locking across concurrent transactions, directly violating the capture throughput goals in [ADR-0008](0008-history-lookup-index.md).
* **Race condition with `clock_timestamp()`:** Because `occurred_at` defaults to `clock_timestamp()`, a transaction that starts before `00:00:00Z` on the first of the month and commits minutes later makes new rows visible in the previous month's `changes_YYYY_MM` partition after the month boundary. Moving rows out of `changes_default` also mutates a past partition. Sealing monthly partitions by calendar boundary therefore produces false-positive tamper alerts.
* **Collision with retention and erasure:** Monthly partitions exist so expired audit data can be dropped cheaply via `DROP TABLE changes_YYYY_MM`. Cryptographic chains spanning stored rows conflict with retention drops and right-to-erasure workflows (see EDPB Guidelines 02/2025).
* **Industry alignment:** Compliance frameworks (PCI-DSS v4.0.1 Req 10.3.2–10.3.4, SOC 2, ISO/IEC 27001:2022 A.8.15, NIST SP 800-92) prioritize write-once storage separation (WORM) and externally signed log digests (as in AWS CloudTrail Log File Integrity Validation). AWS discontinued Amazon QLDB in July 2025 and directs ledger and audit workloads toward PostgreSQL with S3 Object Lock.
* **Approved alternative:** Rely on PostgreSQL role separation (`auditstream_owner`) in the database, and design compliance tamper-evidence around a future WORM-capable object storage sink (S3 Object Lock) with rotated immutable export files, externally signed SHA-256 manifests (Ed25519 or KMS), and offline verification.

### Consequences

* Good, because `auditstream-js` stays zero-dependency, ESM-only, and strictly server-side under one Changesets `fixed` group.
* Good, because users get a realistic, BFF-safe Activity Feed starting point they own and style with their own UI stack, without Auditstream carrying React/CSS SemVer liability.
* Good, because write capture incurs zero hash-chain locking overhead and monthly partition drops continue working without invalidating stored proofs.
* Good, because future tamper-evidence work aligns with auditor-recognized WORM + external asymmetric signing patterns rather than same-trust-domain database hashes.
* Bad, because teams wanting a drop-in React admin widget must copy and adapt the example code instead of installing a single npm package.
* Bad, because teams requiring cryptographic tamper-evidence out of the box must wait for the S3/WORM sink and signed export manifest design or anchor exported files in their own WORM storage today.

## More Information

* [Actor and Context Schema (IDs-only privacy rule)](../../spec/actor.md#privacy-rule-ids-only)
* [History Table and Functions Contract](../../spec/history-table.md)
* [ADR-0006: ESM-only TypeScript packages sharing one context](0006-esm-only-typescript-packages-sharing-one-context.md)
* [ADR-0008: History table lookup index shape](0008-history-lookup-index.md)
* [Capture design](../handbook/capture-design.md)
* [Adapters (TypeScript)](../handbook/adapters.md)
* [AWS CloudTrail Log File Integrity Validation](https://docs.aws.amazon.com/awscloudtrail/latest/userguide/cloudtrail-log-file-validation-intro.html)
* [Amazon S3 Object Lock (WORM)](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html)
* [AWS: Migrate an Amazon QLDB ledger to Amazon Aurora PostgreSQL](https://aws.amazon.com/blogs/database/migrate-an-amazon-qldb-ledger-to-amazon-aurora-postgresql/)
* [EDPB Guidelines 02/2025 on processing personal data through blockchain technologies](https://www.edpb.europa.eu/our-work-tools/our-documents/public-consultations/2025/guidelines-022025-processing-personal-data_en)
* [PCI SSC Document Library (PCI-DSS v4.0.1)](https://www.pcisecuritystandards.org/document_library/)
* [NIST SP 800-92: Guide to Computer Security Log Management](https://csrc.nist.gov/pubs/sp/800/92/final)
* [WorkOS Audit Logs](https://workos.com/docs/audit-logs)
* [BoxyHQ / Retraced `logs-viewer`](https://github.com/retracedhq/logs-viewer)
