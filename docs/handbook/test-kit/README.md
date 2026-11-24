# Auditstream test kit

The field-tested prototype behind the handbook's evidence. It passed on PostgreSQL 15 to 19, behind
PgBouncer 1.26 in transaction mode, on Neon (HTTP, WebSocket and the Neon pooler) and on Supabase
Supavisor. Issue A4 copies it to `docs/handbook/test-kit/` unchanged, and the capture and adapter
issues cite it as the behavior to reproduce.

| Path | What it is |
| --- | --- |
| `sql/schema.sql` | Prototype history schema, roles, capture functions and triggers. Re-runnable; reuses roles |
| `sql/t_app.sql`, `sql/expected_capture.txt`, `sql/capture.sql` | SQL suite run as the application role, and the 17 rows it must produce |
| `sql/bench*.sql`, `sql/bench.sh` | pgbench scripts: comment, set_config, plain transaction, and the leak test |
| `js/` | Prisma 7, Drizzle 0.45 and 1.0, postgres.js and Neon harnesses with probes and concurrency checks |
| `RUNBOOK.md` | The runbook a local agent followed to test Neon and Supabase |

## Known differences from the final design

The prototype is evidence, not the implementation. These points are fixed by the issues, not here:

- **Statement-level triggers.** The prototype uses row-level INSERT and DELETE triggers. The design uses
  statement-level ones with transition tables (D3).
- **Malformed values.** For `auditstream='!!!notbase64'` the prototype regex does not match, so the row gets
  `actor_source` NULL. The spec proposes `invalid` (B3), and the fixtures follow the spec (B6).
- **pg wrapper leak (`js/common.ts`).** An actor-less `pool.query` that waits for a busy pool can be tagged
  with the actor of the request that released the connection, because the per-client wrapper reads the
  context at `client.query` time. JD1 fixes it.
- **postgres.js prepared statements (`js/common.ts`).** Tagged statements must be sent with
  `prepare: false`; otherwise each distinct comment becomes a new prepared statement. JD3 fixes it.
- **Self-grant.** `sql/schema.sql` leaves the admin a member of `auditstream_owner`. The generated
  migration revokes that grant at the end (D2).
