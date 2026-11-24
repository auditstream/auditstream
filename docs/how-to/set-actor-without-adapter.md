# Set the Actor Without an Adapter

You can attribute writes from `psql` or any language driver using either a trailing SQL comment or `set_config`.
Keep payloads to IDs only and under 512 UTF-8 bytes.

## Option 1: Trailing SQL comment (recommended)

Serialize the actor object as compact JSON and encode its UTF-8 bytes as unpadded base64url (`A-Z`, `a-z`, `0-9`, `-`, `_`).

For `{"type":"user","id":"usr_42"}`, the base64url value is `eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0`.
Append `/*auditstream='<value>'*/` at the end of the SQL statement:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -c "INSERT INTO public.invoice (id, status) VALUES (10, 'open') /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;"
```

Trailing comments work without an explicit transaction and do not pin sessions behind connection poolers or RDS Proxy.
Never combine per-actor comments with named prepared statements; use `set_config` instead when named prepared statements are required.

## Option 2: Transaction-local `set_config`

Inside an explicit transaction, call `set_config('auditstream.actor', '<json>', true)` before your writes.
Always pass `true` for the third argument (`is_local`) so the actor never leaks across pooled connections:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -v ON_ERROR_STOP=1 -c "
BEGIN;
SELECT set_config('auditstream.actor', '{\"type\":\"user\",\"id\":\"usr_42\"}', true);
UPDATE public.invoice SET status = 'paid' WHERE id = 10;
COMMIT;
"
```

When both `set_config` and a trailing comment are present, `set_config` takes precedence.

## Record application events (`auditstream.log_event`)

Call `auditstream.log_event(action, target, data)` to log domain events that do not modify an audited table:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -c "SELECT auditstream.log_event('invoice.exported', '{\"id\":10}'::jsonb, '{\"format\":\"pdf\"}'::jsonb) /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;"
```

See [spec/actor.md](../../spec/actor.md) and [spec/transport.md](../../spec/transport.md) for the complete payload schema and encoding rules.
