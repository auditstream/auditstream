// DRIVER=pg | neon-ws | neon-http   APP_URL=<app_user through the target>   ADMIN_URL=<admin, direct>
import type { SqlCommenterPlugin } from "@prisma/sqlcommenter";
import { PrismaClient } from "./generated/prisma/client.ts";
import { als, as, b64, user, sleep, env, admin, reset, Run } from "./common.ts";

const DRIVER = process.env.DRIVER ?? "pg";
const url = env("APP_URL");
// The whole Prisma adapter: read the actor, return one sqlcommenter tag.
const auditstream: SqlCommenterPlugin = () => { const a = als.getStore(); return { auditstream: a ? b64(a) : undefined }; };

async function adapter() {
  if (DRIVER === "pg") { const { PrismaPg } = await import("@prisma/adapter-pg"); return new PrismaPg({ connectionString: url }); }
  if (DRIVER === "neon-ws") { const { PrismaNeon } = await import("@prisma/adapter-neon"); return new PrismaNeon({ connectionString: url }); }
  if (DRIVER === "neon-http") { const { PrismaNeonHttp } = await import("@prisma/adapter-neon"); return new PrismaNeonHttp(url, {} as any); }
  throw new Error("DRIVER must be pg | neon-ws | neon-http");
}

async function main() {
  const prisma = new PrismaClient({ adapter: await adapter(), comments: [auditstream] });
  const a = await admin(); const start = await reset(a);
  const run = new Run(`prisma/${DRIVER}`);
  const P = user("probe");
  await run.probe("queryRaw", P, () => as(P, () => prisma.$queryRawUnsafe("SELECT current_query() AS q")));
  const agent = { type: "ai_agent", id: "billing-agent", onBehalfOf: user("u_42") };
  const ids = Array.from({ length: 40 }, (_, i) => i);

  await run.step("create", ["invoice INSERT 1 u_create"], () => as(user("u_create"), () =>
    prisma.invoice.create({ data: { id: 1, status: "open", total: 10 } })));
  await run.step("nested create", ["invoice INSERT 2 u_nested", "line_item INSERT 20 u_nested", "line_item INSERT 21 u_nested"], () => as(user("u_nested"), () =>
    prisma.invoice.create({ data: { id: 2, status: "open", lineItems: { create: [{ id: 20, qty: 1 }, { id: 21, qty: 2 }] } } })));
  await run.step("createMany", ids.map((i) => `invoice INSERT ${1000 + i} u_many`), () => as(user("u_many"), () =>
    prisma.invoice.createMany({ data: ids.map((i) => ({ id: 1000 + i, status: "new" })) })));
  await run.step("interactive transaction", ["invoice UPDATE 1 billing-agent", "line_item UPDATE 20 billing-agent"], () => as(agent, () =>
    prisma.$transaction(async (tx) => {
      await tx.invoice.update({ where: { id: 1 }, data: { status: "paid" } });
      await sleep(20);
      await tx.lineItem.update({ where: { id: 20 }, data: { qty: 5 } });
    })));
  await run.step("batch transaction", ["invoice UPDATE 2 u_batch", "line_item DELETE 21 u_batch"], () => as(user("u_batch"), () =>
    prisma.$transaction([prisma.invoice.update({ where: { id: 2 }, data: { total: 99 } }), prisma.lineItem.delete({ where: { id: 21 } })])));
  await run.step("raw SQL", ["invoice UPDATE 1 u_raw"], () => as(user("u_raw"), () =>
    prisma.$executeRaw`UPDATE invoice SET status = 'raw' WHERE id = 1`));  // id 1 exists even where nested create is N/A
  await run.step("upsert", ["invoice INSERT 3 u_upsert"], () => as(user("u_upsert"), () =>
    prisma.invoice.upsert({ where: { id: 3 }, create: { id: 3, status: "upserted" }, update: { status: "upserted2" } })));
  await run.step("updateMany", ["invoice UPDATE 1 u_updmany", "invoice UPDATE 3 u_updmany"], () => as(user("u_updmany"), () =>
    prisma.invoice.updateMany({ where: { id: { in: [1, 3] } }, data: { total: 7 } })));
  await run.step("outside any actor scope", ["invoice UPDATE 3 -"], () => prisma.invoice.update({ where: { id: 3 }, data: { status: "no-actor" } }));
  // 40 concurrent requests, each with its own actor, interleaved by random delays (the HTTP driver has no interactive transactions)
  await run.step("40 concurrent requests", ids.flatMap((i) => [`invoice UPDATE ${1000 + i} c${i}`, `invoice UPDATE ${1000 + i} c${i}`]), () =>
    Promise.all(ids.map((i) => as(user(`c${i}`), async () => {
      await sleep(Math.random() * 30);
      await prisma.invoice.update({ where: { id: 1000 + i }, data: { status: `c${i}` } });
      await sleep(Math.random() * 30);
      if (DRIVER === "neon-http") await prisma.invoice.update({ where: { id: 1000 + i }, data: { total: i } });
      else await prisma.$transaction(async (tx) => { await sleep(Math.random() * 10); await tx.invoice.update({ where: { id: 1000 + i }, data: { total: i } }); });
    }))));
  await run.step("delete with cascade", ["invoice DELETE 2 u_delete", "line_item DELETE 20 u_delete"], () => as(user("u_delete"), () =>
    prisma.invoice.delete({ where: { id: 2 } })));

  await run.finish(a, start);
  await prisma.$disconnect(); await a.end();
}
main().catch((e) => { console.error(e); process.exit(1); });
