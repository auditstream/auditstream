# Auditstream Documentation

> Read the hosted documentation at **[auditstream.dev/docs](https://auditstream.dev/docs)**, or browse the source pages in this directory.

Welcome to the Auditstream v0.1 user guide.
These pages follow the Diátaxis structure: tutorials, how-to guides, reference, and explanation.

## Tutorials

Learn Auditstream step by step from an empty PostgreSQL database.

- [Record your first audit trail](tutorials/first-audit-trail.md)

## How-to guides

Task-oriented recipes for integrating and operating Auditstream.

- [Add Auditstream to a Prisma project](how-to/add-to-prisma.md)
- [Add Auditstream to a Drizzle project](how-to/add-to-drizzle.md)
- [Add Auditstream to a plain SQL project](how-to/add-to-plain-sql.md)
- [Audit AI agent mutations](how-to/audit-ai-agents.md)
- [Set the actor without an adapter](how-to/set-actor-without-adapter.md)
- [Export incrementally to JSON Lines or CSV](how-to/export-incrementally.md)
- [Isolate tenants with Row-Level Security (RLS)](how-to/multi-tenant-rls.md)
- [Minimize PII and enforce retention under LGPD and GDPR](how-to/lgpd-and-gdpr-compliance.md)
- [Run on Neon or Supabase](how-to/run-on-neon-and-supabase.md)
- [Check triggers in CI with `verify`](how-to/verify-in-ci.md)

## Reference

Exact specifications of commands, configuration, exit codes, and database objects.

- [Reference index](reference/README.md)
- [CLI reference](reference/cli/README.md)
- [Configuration reference](reference/config.md)
- [Exit codes](reference/exit-codes.md)
- [History table and `log_event`](reference/history-table.md)

## Explanation

Architecture, trade-offs, security model, and performance characteristics.

- [How capture works](explanation/how-capture-works.md)
- [Security model and threat-model boundaries](explanation/security-model.md)

## Project documentation

- [Engineering handbook](handbook/README.md)
- [Normative specification (0.1.1)](../spec/README.md)
- [Architecture Decision Records (ADRs)](decisions/README.md)
