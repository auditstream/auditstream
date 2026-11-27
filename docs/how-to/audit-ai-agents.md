# Audit AI Agent Mutations

Auditstream is a **transactional PostgreSQL audit trail — built for humans and AI agents**.
PostgreSQL triggers capture every database mutation automatically.
Your TypeScript app attributes *which* agent acted and *which* user it represented via `actor.type = "ai_agent"` and `onBehalfOf` — already shipped in `spec v0.1.0` and `@auditstream/node`.

You do not need a framework-specific AI package (see [ADR-0006](../decisions/0006-esm-only-typescript-packages-sharing-one-context.md) and [ADR-0009](../decisions/0009-no-ui-package-or-in-db-partition-sealing.md)).
Wrap each mutating tool handler with `audit.as()` from `@auditstream/node`.
Your existing `@auditstream/prisma` or `@auditstream/drizzle` adapter carries that actor straight to PostgreSQL.

## Prerequisites

1. Run `auditstream init`, apply the generated migration, and confirm `auditstream verify` passes.
2. Wire `@auditstream/node` together with your database adapter (`@auditstream/prisma` or `@auditstream/drizzle`).
3. List sensitive columns in `capture.exclude` so secrets are never written to `auditstream.changes`.

## Mandatory server-side trust boundary

An LLM can be prompt-injected into fabricating tool arguments.
Resolve `agentId`, `onBehalfOf` (the authenticated user), and `tenant` **only from verified server, session, or OAuth/JWT context**.
Never read actor or tenant identity from LLM tool `arguments` or prompt text.

```ts
// GOOD: actor, onBehalfOf, and tenant come from verified server context.
// The LLM schema exposes only business inputs (invoiceId, status).
const inputSchema = z.object({
  invoiceId: z.number().int().positive(),
  status: z.enum(["open", "paid", "void"]),
});

// BAD: NEVER expose actorId, userId, onBehalfOf, or tenantId in LLM tool schemas
// or read them from tool arguments. Prompt injection can spoof any user or tenant.
```

Enforce two runtime guards before calling `audit.as()`:

- **Fail closed on missing auth:** reject missing, non-string, or whitespace-only identity values before touching the database.
- **Bound `requestId`:** validate correlation IDs (for example `/^[A-Za-z0-9._-]{1,64}$/`) or generate a server UUID so oversized values cannot trigger `PAYLOAD_TOO_LARGE`.

Put `tenant` on the top-level `actor` object rather than repeating it inside `onBehalfOf`.
That saves bytes inside the 512-byte budget and matches `actor->>'tenant'` in application-event RLS policies.

## Why wrap at the leaf tool handler (`AsyncLocalStorage`)

`@auditstream/node` propagates actor context through `AsyncLocalStorage.run()`.
Agent runtimes often invoke tools across async streams (`streamText`), graph checkpoints, or worker queues where an outer `audit.as()` scope can drop or interleave.
Always wrap **`audit.as()` directly inside each mutating tool's leaf handler** and **`await` every ORM query inside the callback**.

## Recipe 1: Vercel AI SDK (`ai` v5+)

Build tools per request with a factory closing over the verified server session.
Nothing about the actor enters `inputSchema`.

```ts
import { randomUUID } from "node:crypto";
import { audit } from "@auditstream/node";
import { tool } from "ai";
import { z } from "zod";
import { prisma } from "./db.js"; // PrismaClient configured with comments: [auditstream()]

export interface VerifiedAgentSession {
  readonly agentId: string;   // e.g. "agt_billing"
  readonly userId: string;    // verified session user ID, e.g. "usr_42"
  readonly tenantId: string;  // verified tenant ID, e.g. "org_acme"
  readonly requestId?: string;
}

const REQUEST_ID_RE = /^[A-Za-z0-9._-]{1,64}$/;

function requireNonEmptyId(value: unknown, field: string): string {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`Unauthorized: missing verified ${field}`);
  }
  return value.trim();
}

function resolveRequestId(value: unknown): string {
  return typeof value === "string" && REQUEST_ID_RE.test(value)
    ? value
    : `ai_${randomUUID()}`;
}

export function createBillingTools(session: VerifiedAgentSession) {
  const agentId = requireNonEmptyId(session.agentId, "agentId");
  const userId = requireNonEmptyId(session.userId, "userId");
  const tenantId = requireNonEmptyId(session.tenantId, "tenantId");
  const requestId = resolveRequestId(session.requestId);

  return {
    updateInvoiceStatus: tool({
      description: "Update the status of an invoice belonging to the caller's tenant.",
      inputSchema: z.object({
        invoiceId: z.number().int().positive(),
        status: z.enum(["open", "paid", "void"]),
      }),
      execute: async ({ invoiceId, status }) => {
        return await audit.as(
          {
            type: "ai_agent",
            id: agentId,
            onBehalfOf: { type: "user", id: userId },
            tenant: tenantId,
          },
          async () => {
            // Scope writes to tenantId (if relying on PostgreSQL RLS instead, run
            // set_config('app.tenant', tenantId, true) in the same transaction).
            const updated = await prisma.invoice.updateMany({
              where: { id: invoiceId, tenantId },
              data: { status },
            });
            if (updated.count === 0) {
              throw new Error("Invoice not found in caller tenant");
            }
            return { invoiceId, status };
          },
          { context: { requestId, source: "ai-tool" } },
        );
      },
    }),
  };
}
```

Under **Prisma Accelerate** (or proxies that strip SQL comments), wrap the write inside `auditTransaction(prisma, async (tx) => ...)` from `@auditstream/prisma` so the actor travels via transaction-local `set_config` instead of comments.

## Recipe 2: LangGraph (`@langchain/core/tools`)

Pass verified identity through LangGraph's server-side `configurable` config — never through the LLM-visible `schema` or prompt messages.
Extract validated local `const` strings before entering `audit.as()` so TypeScript preserves narrowing inside the closure.

> **LangGraph Server / Platform warning:** On hosted LangGraph Server, HTTP clients can send arbitrary `config.configurable` fields unless custom auth middleware (`auth.authenticate` / `auth.on`) overwrites identity keys or the tool reads `config.configurable.langgraph_auth_user`. Never trust client-supplied `configurable` identity fields.

```ts
import { randomUUID } from "node:crypto";
import { audit } from "@auditstream/node";
import { tool, type ToolRunnableConfig } from "@langchain/core/tools";
import { and, eq } from "drizzle-orm";
import { z } from "zod";
// Drizzle wired with wrapPgPool, wrapPostgresJs, or wrapNeonHttp
// (Drizzle 1.x setups without a driver wrapper must chain .comment(auditComment())).
import { db, invoice } from "./db.js";

const REQUEST_ID_RE = /^[A-Za-z0-9._-]{1,64}$/;

function readString(obj: Record<string, unknown>, key: string): string | undefined {
  const val = obj[key];
  return typeof val === "string" && val.trim().length > 0 ? val.trim() : undefined;
}

export const updateInvoiceStatusTool = tool(
  async ({ invoiceId, status }, config?: ToolRunnableConfig) => {
    const raw = (config?.configurable ?? {}) as Record<string, unknown>;
    const agentId = readString(raw, "agentId");
    const userId = readString(raw, "userId");
    const tenantId = readString(raw, "tenantId");

    if (!agentId || !userId || !tenantId) {
      throw new Error("Unauthorized: missing verified server identity in LangGraph config");
    }

    const rawReqId = readString(raw, "requestId");
    const requestId = rawReqId && REQUEST_ID_RE.test(rawReqId)
      ? rawReqId
      : `lg_${randomUUID()}`;

    return await audit.as(
      {
        type: "ai_agent",
        id: agentId,
        onBehalfOf: { type: "user", id: userId },
        tenant: tenantId,
      },
      async () => {
        const rows = await db
          .update(invoice)
          .set({ status })
          .where(and(eq(invoice.id, invoiceId), eq(invoice.tenantId, tenantId)))
          .returning({ id: invoice.id, status: invoice.status });

        const [updated] = rows;
        if (!updated) {
          throw new Error("Invoice not found in caller tenant");
        }
        return JSON.stringify(updated);
      },
      { context: { requestId, source: "ai-tool" } },
    );
  },
  {
    name: "update_invoice_status",
    description: "Update the status of an invoice belonging to the caller's tenant.",
    schema: z.object({
      invoiceId: z.number().int().positive(),
      status: z.enum(["open", "paid", "void"]),
    }),
  },
);
```

When invoking the graph from your authenticated API handler, populate `configurable` strictly from the verified request session:

```ts
await graph.invoke(
  { messages: [{ role: "user", content: prompt }] },
  {
    configurable: {
      thread_id: session.threadId,
      agentId: "agt_billing",
      userId: session.userId,
      tenantId: session.tenantId,
      requestId: session.requestId,
    },
  },
);
```

## Recipe 3: Write-side MCP server (`@modelcontextprotocol/sdk`)

On remote Streamable HTTP MCP servers, attach verified OAuth bearer claims to `extra.authInfo` via transport auth middleware (`requireBearerAuth`).
Always construct a **fresh `McpServer` instance per session or request** via a factory function — reusing a singleton `McpServer` across Streamable HTTP clients leaks cross-client responses on `@modelcontextprotocol/sdk` `1.10.0`–`1.25.3` (CVE-2026-25536) and throws on `>= 1.26.0`.

*(Note: MCP SDK v2 / `@modelcontextprotocol/server` nests HTTP auth at `ctx.http?.authInfo`, moves JSON-RPC IDs to `ctx.mcpReq.id`, and expects `inputSchema: z.object({ ... })`. Local `StdioServerTransport` sessions leave `authInfo` undefined and belong only in trusted single-user local environments.)*

```ts
import { randomUUID } from "node:crypto";
import { audit } from "@auditstream/node";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { prisma } from "./db.js"; // PrismaClient configured with comments: [auditstream()]

export function createBillingMcpServer(): McpServer {
  const server = new McpServer({
    name: "billing-mcp",
    version: "1.0.0",
  });

  server.registerTool(
    "update_invoice_status",
    {
      title: "Update invoice status",
      description: "Update the status of an invoice in the authenticated caller's tenant.",
      inputSchema: {
        invoiceId: z.number().int().positive(),
        status: z.enum(["open", "paid", "void"]),
      },
    },
    async ({ invoiceId, status }, extra) => {
      const authInfo = extra.authInfo;
      const agentId = typeof authInfo?.clientId === "string" ? authInfo.clientId.trim() : "";
      const userId = typeof authInfo?.extra?.userId === "string" ? authInfo.extra.userId.trim() : "";
      const tenantId = typeof authInfo?.extra?.tenantId === "string" ? authInfo.extra.tenantId.trim() : "";
      const scopes = Array.isArray(authInfo?.scopes) ? authInfo.scopes : [];

      if (!authInfo || !agentId || !userId || !tenantId) {
        throw new Error("Unauthorized: verified MCP OAuth context is required for writes");
      }
      if (!scopes.includes("invoices:write")) {
        throw new Error("Forbidden: missing invoices:write scope");
      }

      // Generate a server-owned correlation ID (or read an active server OTel span ID).
      // Only read x-request-id if a trusted ingress proxy strips and overwrites it on every request;
      // never trust client-chosen JSON-RPC extra.requestId (string | number, session-scoped).
      const requestId = `mcp_${randomUUID()}`;

      // Attach requestId to your server logger / OTel span for trace correlation.
      const result = await audit.as(
        {
          type: "ai_agent",
          id: agentId,
          onBehalfOf: { type: "user", id: userId },
          tenant: tenantId,
        },
        async () => {
          // Under Prisma Accelerate, use auditTransaction(prisma, async (tx) => ...).
          const updated = await prisma.invoice.updateMany({
            where: { id: invoiceId, tenantId },
            data: { status },
          });
          if (updated.count === 0) {
            throw new Error("Invoice not found in caller tenant");
          }
          return { invoiceId, status };
        },
        { context: { requestId, source: "mcp" } },
      );

      return {
        content: [{ type: "text", text: JSON.stringify(result) }],
      };
    },
  );

  return server;
}
```

## Privacy boundary, 512-byte budget, and observability correlation

Auditstream enforces an **IDs-only, 512-byte compact UTF-8 JSON** limit on the combined `actor` and `context` payload ([`spec/actor.md`](../../spec/actor.md#privacy-rule-ids-only)):

- **Never store prompts or tool I/O in `actor` or `context`:** trailing SQL comments appear in `pg_stat_activity`, server logs, and `pg_stat_statements`. Exceeding 512 UTF-8 bytes fails with `PAYLOAD_TOO_LARGE` instead of truncating.
- **Correlate with your LLM observability store:** keep prompts, completions, and tool arguments in Langfuse, Braintrust, or OpenTelemetry. Pass the shared correlation ID in `context.requestId` today so responders can join PostgreSQL row diffs (`before`/`after`/`changed`) to the LLM trace.
- **Planned v0.2 (`#146`):** native `context.traceId` and `context.spanId` fields plus `--trace-id`, `--actor-type`, and `--xact-id` history filters are tracked in [#146](https://github.com/auditstream/auditstream/issues/146).
- **`onBehalfOf` nesting limit:** `onBehalfOf` nests strictly one level deep (`ai_agent` acting for `user`). In multi-agent chains, record the executing leaf agent in `actor.id` and the originating human in `onBehalfOf`, and track the full call tree via `requestId`.

## Inspecting AI agent mutations

Use `auditstream history` to inspect what an agent changed or what happened on a user's behalf (`--actor` matches both `actor.id` and `actor.onBehalfOf.id`):

```sh
# All mutations performed by billing agent (with field-level diffs)
auditstream history --actor agt_billing --since 2026-11-01T00:00:00Z --diff

# All mutations performed by or on behalf of usr_42 (human + delegated AI agents)
auditstream history --actor usr_42 --since 2026-11-01T00:00:00Z

# Payload-free timeline filtered by actor.tenant metadata (use PostgreSQL RLS for tenant authorization)
auditstream history --actor agt_billing --tenant org_acme --since 2026-11-01T00:00:00Z --omit-payload --format json
```

Note that `--omit-payload` drops `before` and `after` while still emitting `record_id` (never redacted) and metadata (`db_user`, `context`).
Unlike `--format table` and `--diff`, `--format json` preserves raw JSON strings without terminal BiDi/control escaping; treat raw JSON output as untrusted data before passing it back to an LLM or terminal.

When showing agent activity in your own UI, query via `audit.query()` on the server and enforce tenant authorization (or PostgreSQL RLS — see [Isolate tenants with Row-Level Security (RLS)](multi-tenant-rls.md)).
Always project an allowlisted DTO before returning rows to browsers ([ADR-0009](../decisions/0009-no-ui-package-or-in-db-partition-sealing.md)).
