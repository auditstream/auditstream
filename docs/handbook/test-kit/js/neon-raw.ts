// Neon only. APP_URL=<app_user, Neon host>   ADMIN_URL=<neondb_owner, direct host>
// The set_config path and leaks over the Neon HTTP and WebSocket drivers, without an ORM.
import { neon, Pool } from "@neondatabase/serverless";
import { b64, user, env, admin, reset, Run } from "./common.ts";

const url = env("APP_URL");
const tagged = (q: string, id: string) => `${q} /*auditstream='${b64(user(id))}'*/`;
const actorJson = (id: string) => JSON.stringify(user(id));

async function main() {
  const a = await admin(); const start = await reset(a);
  const run = new Run("neon-raw");
  await a.query("INSERT INTO invoice (id, status) VALUES (1, 'x'), (2, 'x'), (3, 'x'), (4, 'x')");
  run.expected.push(...[1, 2, 3, 4].map((i) => `invoice INSERT ${i} -`));

  const http = neon(url);
  await run.probe("http", user("probe"), () => http.query(tagged("SELECT current_query() AS q", "probe")));
  await run.step("HTTP: statement with comment", ["invoice UPDATE 1 http_comment"], () =>
    http.query(tagged("UPDATE invoice SET status = 'h1' WHERE id = 1", "http_comment")));
  await run.step("HTTP: transaction with set_config", ["invoice UPDATE 2 http_setcfg"], () =>
    http.transaction([http`SELECT set_config('auditstream.actor', ${actorJson("http_setcfg")}, true)`, http`UPDATE invoice SET status = 'h2' WHERE id = 2`]));
  await run.step("HTTP: next statement has no actor (no leak)", ["invoice UPDATE 2 -"], () =>
    http.query("UPDATE invoice SET status = 'h3' WHERE id = 2"));

  const pool = new Pool({ connectionString: url, max: 1 });
  await run.probe("websocket", user("probe"), () => pool.query(tagged("SELECT current_query() AS q", "probe")));
  await run.step("WebSocket: transaction with set_config, then a plain statement on the same connection",
    ["invoice UPDATE 3 ws_setcfg", "invoice UPDATE 3 -"], async () => {
      const c = await pool.connect();
      try {
        await c.query("BEGIN");
        await c.query("SELECT set_config('auditstream.actor', $1, true)", [actorJson("ws_setcfg")]);
        await c.query("UPDATE invoice SET status = 'w1' WHERE id = 3");
        await c.query("COMMIT");
        await c.query("UPDATE invoice SET status = 'w2' WHERE id = 3");
      } finally { c.release(); }
    });
  await run.step("WebSocket: statement with comment", ["invoice UPDATE 4 ws_comment"], () =>
    pool.query(tagged("UPDATE invoice SET status = 'w3' WHERE id = 4", "ws_comment")));

  await run.finish(a, start);
  await pool.end(); await a.end();
}
main().catch((e) => { console.error(e); process.exit(1); });
