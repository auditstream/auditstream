# Record Your First Audit Trail

This tutorial takes you from an empty PostgreSQL container to a captured row change with an actor.
You only need Docker, `psql`, and the `auditstream` CLI.
No Node.js setup is required.

## Install the CLI

Install the prebuilt `auditstream` v0.1.0 CLI through your preferred channel:

```sh
# Shell (macOS / Linux)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/auditstream/auditstream/releases/latest/download/auditstream-installer.sh | sh

# Homebrew
brew install auditstream/tap/auditstream

# npm
npm i -g auditstream

# Docker
docker run --rm ghcr.io/auditstream/auditstream:0.1.0 --help
```

On Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/auditstream/auditstream/releases/latest/download/auditstream-installer.ps1 | iex"
```

Or build from source with Cargo as a fallback:

```sh
cargo install --path crates/auditstream --locked
```

## 1. Start PostgreSQL in Docker

```sh
docker run -d --rm --name as-guide -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:18
```

Wait two seconds for PostgreSQL to accept connections, then export your connection string:

```sh
export AUDITSTREAM_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5432/postgres"
```

## 2. Create a sample table

```sh
psql "$AUDITSTREAM_DATABASE_URL" -c "CREATE TABLE public.invoice (id int PRIMARY KEY, status text NOT NULL, password_hash text);"
```

## 3. Generate the audit migration

In an empty directory, run `auditstream init`:

```sh
auditstream init --migrations sql --table public.invoice --exclude public.invoice.password_hash
```

`init` writes a SQL migration under `migrations/` and saves your table selection in `auditstream.toml`.
It never writes `database_url` into `auditstream.toml`.

## 4. Apply the migration and verify triggers

```sh
psql "$AUDITSTREAM_DATABASE_URL" -v ON_ERROR_STOP=1 -f migrations/*_auditstream.sql
auditstream verify
```

`auditstream verify` prints `ok: no findings` and exits `0`.

## 5. Write a row with an actor comment

Encode `{"type":"user","id":"usr_42"}` as unpadded base64url: `eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0`.
Attach it as a trailing SQL comment on your write:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -c "INSERT INTO public.invoice (id, status, password_hash) VALUES (1, 'draft', 'secret') /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;"
```

Update the row to record a diff:

```sh
psql "$AUDITSTREAM_DATABASE_URL" -c "UPDATE public.invoice SET status = 'sent' WHERE id = 1 /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;"
```

## 6. Inspect the audit trail

```sh
auditstream history --table public.invoice --record-id 1
```

You see both the `UPDATE` (with `changed = status`) and `INSERT`, newest first, attributed to `user:usr_42` via `comment`.

## Optional: inspect full JSON and clean up

Pass `--format json` to inspect full `before` and `after` images and confirm `password_hash` was excluded:

```sh
auditstream history --table public.invoice --record-id 1 --format json
docker stop as-guide
```

## Next steps

In real applications, use a TypeScript adapter so every request carries its actor automatically:

- [Prisma adapter (`@auditstream/prisma`)](https://github.com/auditstream/auditstream-js/blob/main/packages/prisma/README.md)
- [Drizzle adapter (`@auditstream/drizzle`)](https://github.com/auditstream/auditstream-js/blob/main/packages/drizzle/README.md)
- [Core Node context (`@auditstream/node`)](https://github.com/auditstream/auditstream-js/blob/main/packages/node/README.md)

Or explore the guides:

- [Set the actor without an adapter](../how-to/set-actor-without-adapter.md)
- [Export incrementally to JSON Lines or CSV](../how-to/export-incrementally.md)
- [How capture works](../explanation/how-capture-works.md)
