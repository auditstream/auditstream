import { AsyncLocalStorage } from "node:async_hooks";
import pg from "pg";

export type Actor = { type: string; id: string; onBehalfOf?: Actor };
export const als = new AsyncLocalStorage<Actor>();
export const b64 = (a: Actor) => Buffer.from(JSON.stringify(a)).toString("base64url");
// Await INSIDE the scope: Prisma and Neon queries are lazy and only run on .then().
export const as = <T>(actor: Actor, fn: () => Promise<T>) => als.run(actor, async () => await fn());
export const user = (id: string): Actor => ({ type: "user", id });
export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
export const env = (k: string) => { const v = process.env[k]; if (!v) throw new Error(`${k} is not set`); return v; };

const TAG = /\/\*auditstream='[A-Za-z0-9_-]+'\*\/$/;
export const tagText = (q: string, a: Actor | undefined = als.getStore()) =>
  a && !TAG.test(q) ? `${q} /*auditstream='${b64(a)}'*/` : q;

// pg.Pool and the Neon WebSocket Pool: tag where the caller is (pool.query entry) and on checked-out clients.
// pg-pool can run a queued callback in another request's async context, so the actor is read at entry.
function tagConfig(config: any, a: Actor | undefined) {
  if (!a) return config;
  if (typeof config === "string") return tagText(config, a);
  if (config && typeof config.text === "string" && !TAG.test(config.text)) {
    const c = { ...config, text: tagText(config.text, a) };
    delete c.name; // a tagged statement is never sent as a named prepared statement
    return c;
  }
  return config;
}
export function wrapPgPool<P>(pool: P): P {
  const p = pool as any;
  const poolQuery = p.query.bind(p);
  p.query = (config: any, values?: any, cb?: any) => poolQuery(tagConfig(config, als.getStore()), values, cb);
  p.on("connect", (client: any) => {
    const original = client.query;
    client.query = function (config: any, values?: any, cb?: any) { return original.call(this, tagConfig(config, als.getStore()), values, cb); };
  });
  return pool;
}

// postgres.js: Drizzle hands statements to unsafe(), on the root client and inside begin/savepoint.
export function wrapPostgresJs(sql: any): any {
  const unsafe = (t: any) => (q: string, params?: any, opts?: any) => t.unsafe(tagText(q), params, opts);
  const wrapTx = (tx: any): any => new Proxy(tx, { get(t, p, r) {
    if (p === "unsafe") return unsafe(t);
    if (p === "savepoint") return (...args: any[]) => { const fn = args.at(-1); args[args.length - 1] = (s: any) => fn(wrapTx(s)); return t.savepoint(...args); };
    return Reflect.get(t, p, r);
  } });
  return new Proxy(sql, { get(t, p, r) {
    if (p === "unsafe") return unsafe(t);
    if (p === "begin") return (...args: any[]) => { const fn = args.at(-1); args[args.length - 1] = (tx: any) => fn(wrapTx(tx)); return t.begin(...args); };
    return Reflect.get(t, p, r);
  } });
}

// Neon HTTP: Drizzle calls client.query(text, params, opts) and client.transaction([...]).
export function wrapNeonHttp(sql: any): any {
  const query = (text: string, params?: any[], opts?: any) => sql.query(tagText(text), params, opts);
  return new Proxy(sql, {
    apply(t, thisArg, args) { return typeof args[0] === "string" ? query(args[0], args[1], args[2]) : Reflect.apply(t, thisArg, args); },
    get(t, p, r) { return p === "query" ? query : Reflect.get(t, p, r); },
  });
}

export async function admin() { const c = new pg.Client({ connectionString: env("ADMIN_URL") }); await c.connect(); return c; }
export async function reset(a: pg.Client) {
  await a.query("DELETE FROM line_item; DELETE FROM invoice;");
  return (await a.query("SELECT coalesce(max(id), 0)::text AS m FROM auditstream.changes")).rows[0].m as string;
}
// PROBE: did the trailing comment reach current_query() unchanged on this path?
export const probe = (result: any, a: Actor): true | string => {
  const q = (result?.rows ?? result)?.[0]?.q;
  return typeof q === "string" && q.trimEnd().endsWith(`/*auditstream='${b64(a)}'*/`) ? true : `got: ${String(q).slice(-160)}`;
};

export class Run {
  expected: string[] = []; notes: string[] = []; probes: Record<string, true | string> = {};
  constructor(public label: string) {}
  async probe(name: string, a: Actor, fn: () => Promise<unknown>) {
    try { this.probes[name] = probe(await fn(), a); }
    catch (e: any) { this.probes[name] = `error: ${String(e?.message ?? e).trim().split("\n")[0].slice(0, 200)}`; }
  }
  async step(name: string, rows: string[], fn: () => Promise<unknown>) {
    try { await fn(); this.expected.push(...rows); }
    catch (e: any) { this.notes.push(`${name}: N/A (${String(e?.message ?? e).trim().split("\n")[0].slice(0, 200)})`); }
  }
  async finish(a: pg.Client, start: string) {
    const rows = (await a.query(
      `SELECT table_name AS t, op, record_id->>'id' AS pk, actor->>'id' AS actor, actor_source AS src
       FROM auditstream.changes WHERE id > $1 AND table_name IN ('invoice', 'line_item') ORDER BY id`, [start])).rows;
    const got = rows.map((r) => `${r.t} ${r.op} ${r.pk} ${r.actor ?? "-"}`);
    const left = [...got]; const missing: string[] = [];
    for (const e of this.expected) { const i = left.indexOf(e); if (i >= 0) left.splice(i, 1); else missing.push(e); }
    const sources: Record<string, number> = {};
    for (const r of rows) sources[r.src ?? "none"] = (sources[r.src ?? "none"] ?? 0) + 1;
    const ok = Object.values(this.probes).every((p) => p === true) && missing.length === 0 && left.length === 0;
    console.log(JSON.stringify({ label: this.label, result: ok ? "PASS" : "FAIL", probes: this.probes,
      expectedRows: this.expected.length, capturedRows: got.length, missing: missing.slice(0, 20), unexpected: left.slice(0, 20),
      actorSources: sources, notes: this.notes }, null, 2));
  }
}
