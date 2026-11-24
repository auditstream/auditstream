// MODE=pg | pgjs-prepare | pgjs-noprepare | neon-ws | neon-http   APP_URL=<app_user through the target>   ADMIN_URL=<admin, direct>
import pg from "pg";
import postgres from "postgres";
import { neon, Pool as NeonPool } from "@neondatabase/serverless";
import { pgTable, integer, text } from "drizzle-orm/pg-core";
import { eq, sql } from "drizzle-orm";
import { pgTable as pgTableRc, integer as intRc, text as textRc } from "drizzle-rc/pg-core";
import { eq as eqRc } from "drizzle-rc";
import { as, b64, user, sleep, env, admin, reset, Run, wrapPgPool, wrapPostgresJs, wrapNeonHttp } from "./common.ts";

const MODE = process.env.MODE ?? "pg";
const url = env("APP_URL");
const inv = pgTable("invoice", { id: integer().primaryKey(), status: text() });
const invRc = pgTableRc("invoice", { id: intRc().primaryKey(), status: textRc() });

// db: Drizzle 0.45 behind the Auditstream driver wrapper. rc: Drizzle 1.0 RC with no wrapper, using .comment().
async function clients(): Promise<{ db: any; rc: any; interactiveTx: boolean }> {
  if (MODE === "pg") {
    const { drizzle } = await import("drizzle-orm/node-postgres"); const { drizzle: rc } = await import("drizzle-rc/node-postgres");
    return { db: drizzle({ client: wrapPgPool(new pg.Pool({ connectionString: url, max: 5 })) }), rc: rc({ client: new pg.Pool({ connectionString: url, max: 2 }) }), interactiveTx: true };
  }
  if (MODE === "pgjs-prepare" || MODE === "pgjs-noprepare") {
    const prepare = MODE === "pgjs-prepare";
    const { drizzle } = await import("drizzle-orm/postgres-js"); const { drizzle: rc } = await import("drizzle-rc/postgres-js");
    return { db: drizzle({ client: wrapPostgresJs(postgres(url, { max: 5, prepare })) }), rc: rc({ client: postgres(url, { max: 2, prepare }) }), interactiveTx: true };
  }
  if (MODE === "neon-ws") {
    const { drizzle } = await import("drizzle-orm/neon-serverless"); const { drizzle: rc } = await import("drizzle-rc/neon-serverless");
    return { db: drizzle({ client: wrapPgPool(new NeonPool({ connectionString: url, max: 5 })) }), rc: rc({ client: new NeonPool({ connectionString: url, max: 2 }) }), interactiveTx: true };
  }
  if (MODE === "neon-http") {
    const { drizzle } = await import("drizzle-orm/neon-http"); const { drizzle: rc } = await import("drizzle-rc/neon-http");
    return { db: drizzle({ client: wrapNeonHttp(neon(url)) }), rc: rc({ client: neon(url) }), interactiveTx: false };
  }
  throw new Error("MODE must be pg | pgjs-prepare | pgjs-noprepare | neon-ws | neon-http");
}

async function main() {
  const { db, rc, interactiveTx } = await clients();
  const a = await admin(); const start = await reset(a);
  const run = new Run(`drizzle/${MODE}`);
  const P = user("probe");
  await run.probe("wrapper", P, () => as(P, () => db.execute(sql`SELECT current_query() AS q`)));
  const ids = Array.from({ length: 40 }, (_, i) => i);

  await run.step("insert", ["invoice INSERT 1 d_insert", "invoice INSERT 2 d_insert"], () => as(user("d_insert"), () =>
    db.insert(inv).values([{ id: 1, status: "a" }, { id: 2, status: "b" }])));
  if (interactiveTx) await run.step("transaction with savepoint", ["invoice UPDATE 1 d_tx", "invoice DELETE 2 d_tx"], () => as(user("d_tx"), () =>
    db.transaction(async (tx: any) => {
      await tx.update(inv).set({ status: "tx" }).where(eq(inv.id, 1));
      await sleep(10);
      await tx.transaction(async (sp: any) => { await sp.delete(inv).where(eq(inv.id, 2)); });
    })));
  else await run.step("batch (HTTP transaction)", ["invoice UPDATE 1 d_tx", "invoice DELETE 2 d_tx"], () => as(user("d_tx"), () =>
    db.batch([db.update(inv).set({ status: "tx" }).where(eq(inv.id, 1)), db.delete(inv).where(eq(inv.id, 2))])));
  await run.step("prepared statement", ["invoice UPDATE 1 d_prepared"], async () => {
    const p = db.update(inv).set({ status: "prep" }).where(eq(inv.id, 1)).prepare("upd1");
    await as(user("d_prepared"), () => p.execute());
  });
  await run.step("seed 40 rows without an actor", ids.map((i) => `invoice INSERT ${100 + i} -`), () =>
    db.insert(inv).values(ids.map((i) => ({ id: 100 + i, status: "new" }))));
  await run.step("40 concurrent requests", ids.map((i) => `invoice UPDATE ${100 + i} k${i}`), () =>
    Promise.all(ids.map((i) => as(user(`k${i}`), async () => {
      await sleep(Math.random() * 20);
      await db.update(inv).set({ status: `k${i}` }).where(eq(inv.id, 100 + i));
    }))));
  await run.step("Drizzle 1.0 .comment()", ["invoice INSERT 3 rc_comment", "invoice UPDATE 3 rc_comment"], async () => {
    const tag = { auditstream: b64(user("rc_comment")) };
    await rc.insert(invRc).values({ id: 3, status: "rc" }).comment(tag);
    await rc.update(invRc).set({ status: "rc2" }).where(eqRc(invRc.id, 3)).comment(tag);
  });

  await run.finish(a, start);
  await a.end(); process.exit(0);
}
main().catch((e) => { console.error(e); process.exit(1); });
