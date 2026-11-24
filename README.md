# auditstream

Auditstream is an open-source PostgreSQL audit trail engine written in Rust.
Application code never writes audit rows itself.
Adapters attach the actor to each query, generated triggers record changes in `auditstream.changes`, and the CLI reads and exports history.

**Status:** v0.1 (pre-1.0). Breaking changes bump the minor version; see [Version policy](docs/handbook/releases.md#version-policy).

## How it works

```mermaid
flowchart LR
  App["Application + Adapter"] -- "SQL + actor" --> Tables["PostgreSQL tables"]
  Tables --> Triggers["Generated triggers"]
  Triggers --> Changes["auditstream.changes"]
  CLI["auditstream CLI (Rust)"] -- "init / verify" --> Tables
  Changes --> CLI
  CLI --> Out["JSON Lines / CSV"]
```

## Install

Install the prebuilt `auditstream` CLI through any supported channel:

- **Shell (macOS and Linux):**
  ```sh
  curl --proto '=https' --tlsv1.2 -LsSf https://github.com/auditstream/auditstream/releases/latest/download/auditstream-installer.sh | sh
  ```
- **PowerShell (Windows):**
  ```powershell
  powershell -ExecutionPolicy Bypass -c "irm https://github.com/auditstream/auditstream/releases/latest/download/auditstream-installer.ps1 | iex"
  ```
- **npm:**
  ```sh
  npm install -g auditstream
  ```
- **Homebrew:**
  ```sh
  brew install auditstream/tap/auditstream
  ```
- **Docker:**
  ```sh
  docker run --rm ghcr.io/auditstream/auditstream:latest --help
  ```

Or build from source with Cargo:

```sh
cargo install --path crates/auditstream --locked
```

## Quick start

Run these six commands from an empty directory with Docker, `psql`, and `auditstream` installed:

```sh
docker run -d --rm --name as-readme -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:18
psql postgres://postgres:postgres@127.0.0.1:5432/postgres -c "CREATE TABLE public.invoice (id int PRIMARY KEY, status text NOT NULL, password_hash text);"
auditstream --database-url postgres://postgres:postgres@127.0.0.1:5432/postgres init --migrations sql --table public.invoice --exclude public.invoice.password_hash
psql postgres://postgres:postgres@127.0.0.1:5432/postgres -v ON_ERROR_STOP=1 -f migrations/*_auditstream.sql
psql postgres://postgres:postgres@127.0.0.1:5432/postgres -c "INSERT INTO public.invoice VALUES (1, 'draft', 'secret') /*auditstream='eyJ0eXBlIjoidXNlciIsImlkIjoidXNyXzQyIn0'*/;"
auditstream --database-url postgres://postgres:postgres@127.0.0.1:5432/postgres history --table public.invoice --record-id 1
```

Example output:

```text
OCCURRED_AT                  OP      TABLE           RECORD_ID  ACTOR        ACTOR_SOURCE  CHANGED
2026-11-24T15:22:00.000000Z  INSERT  public.invoice  {"id":1}   user:usr_42  comment       -
```

Stop the container with `docker stop as-readme`.
Follow the full walkthrough in [Record your first audit trail](docs/tutorials/first-audit-trail.md).

## Supported versions

| Dependency | Supported in v0.1 |
| --- | --- |
| PostgreSQL | 15 to 19 (14 reached end of life on 2026-11-12) |
| Node.js | 22.12 or newer; tested on 22, 24, and 26 |
| Prisma | `@prisma/client` 7.1 and newer within 7.x; Prisma 8 after its stable release |
| Drizzle | 0.45 and 1.x |

## Limits

- **Aurora DSQL** has no triggers and cannot run Auditstream v0.1.
- **CockroachDB** is unproven and unsupported.
- **Prisma Accelerate** apps must use transaction-local `set_config` instead of trailing SQL comments.
- **AWS RDS Proxy** pins sessions under `set_config`; use the trailing SQL comment transport there.
- **Write overhead:** single-row updates cost up to 40% of throughput with comments (48% with `set_config`). Bulk writes cost much less per row.
- **Bypasses:** `session_replication_role = replica` and disabled or dropped triggers bypass capture. Run `auditstream verify` to detect trigger drift.

## Documentation

- [User guide](docs/README.md) — tutorials, how-to guides, CLI and schema reference, and capture explanation
- [Engineering handbook](docs/handbook/README.md) — architecture, capture design, evidence, and release process
- [Specification 0.1.0](spec/README.md) — normative actor, transport, history table, and export contract
- [Architecture Decision Records (ADRs)](docs/decisions/README.md) — key technical decisions
- TypeScript adapters in [`auditstream/auditstream-js`](https://github.com/auditstream/auditstream-js):
  - [`@auditstream/node`](https://github.com/auditstream/auditstream-js/blob/main/packages/node/README.md)
  - [`@auditstream/prisma`](https://github.com/auditstream/auditstream-js/blob/main/packages/prisma/README.md)
  - [`@auditstream/drizzle`](https://github.com/auditstream/auditstream-js/blob/main/packages/drizzle/README.md)

## Contributing and security

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
Contributions require no CLA and no DCO.
Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
Participation is governed by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

Auditstream is licensed under the [MIT License](LICENSE).
