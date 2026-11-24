---
status: accepted
date: 2026-11-24
decision-makers: Gabriel Lobo, Cleisson
---

# ESM-only TypeScript packages sharing one context

## Context and Problem Statement

The `auditstream-js` repository publishes `@auditstream/node`, `@auditstream/prisma` and `@auditstream/drizzle`. `@auditstream/node` owns the actor context using `AsyncLocalStorage` (`run()` only), and the ORM adapters read that context to tag Postgres statements. If two copies of `@auditstream/node` load in the same process, they create two separate `AsyncLocalStorage` instances and hide the actor.

## Considered Options

* Ship ESM-only packages (`type: "module"`, no CommonJS build) with `@auditstream/node` declared as a peer dependency of `@auditstream/prisma` and `@auditstream/drizzle`
* Ship dual ESM and CommonJS builds
* Declare `@auditstream/node` as a direct dependency of each ORM adapter

## Decision Outcome

Chosen option: "Ship ESM-only packages with `@auditstream/node` declared as a peer dependency of `@auditstream/prisma` and `@auditstream/drizzle`", because a dual ESM and CommonJS build or duplicate dependency tree can load the package twice and split the `AsyncLocalStorage` context.

Packages are built with `tsdown` 0.23, support Node.js 22.12 or newer (developed on Node 26), and release together as a Changesets `fixed` group.

### Consequences

* Good, because every ORM adapter shares the single `AsyncLocalStorage` instance owned by `@auditstream/node`, preventing dual-package context loss.
* Good, because `run()`-only `AsyncLocalStorage` works across Node, Bun, Deno and Cloudflare Workers (where `enterWith()` does not exist).
* Good, because releasing the three packages as a `fixed` group keeps their versions aligned.
* Bad, because applications must run on Node.js 22.12 or newer (or a supported runtime) and cannot import a CommonJS build.
* Bad, because users of `@auditstream/prisma` or `@auditstream/drizzle` must install `@auditstream/node` explicitly as a peer dependency.

## More Information

* [Adapters (TypeScript)](../handbook/adapters.md)
* [Engineering standards](../handbook/engineering-standards.md)
* [Releases](../handbook/releases.md#adapters)
