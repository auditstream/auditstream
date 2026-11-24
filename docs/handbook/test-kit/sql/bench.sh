#!/usr/bin/env bash
# usage: sql/bench.sh <label> <clients> [seconds]
# Needs ADMIN_URL (admin, direct) and APP_URL (app_user through the pooler), with psql and pgbench on PATH.
# Use more clients than the pooler's server-side pool size, so clients queue for server connections.
set -u
L=$1; C=$2; T=${3:-15}; D=$(cd "$(dirname "$0")" && pwd)
num() { sed -n "s/.*$1[^0-9]*\([0-9][0-9.]*\).*/\1/p" | head -1; }
maxid() { psql "$ADMIN_URL" -Atc "SELECT coalesce(max(id), 0) FROM auditstream.changes"; }
for M in simple extended prepared; do
  for S in comment setcfg; do
    START=$(maxid)
    OUT=$(pgbench -n -M "$M" -c "$C" -j 4 -T "$T" -f "$D/bench_$S.sql" "$APP_URL" 2>&1); RC=$?
    TX=$(num "actually processed" <<<"$OUT"); FAILED=$(num "failed transactions" <<<"$OUT"); TPS=$(num "tps =" <<<"$OUT")
    ROWS=$(psql "$ADMIN_URL" -Atc "SELECT coalesce(string_agg(coalesce(actor_source, 'none') || '=' || n, ' '), 'none') FROM (SELECT actor_source, count(*) n FROM auditstream.changes WHERE id > $START AND table_name = 'up_t' GROUP BY 1) x")
    ERR=$(grep -iE 'error|timeout|abort' <<<"$OUT" | head -2 | tr '\n' ' ')
    echo "$L | -M $M | $S | exit=$RC tx=${TX:-?} failed=${FAILED:-?} tps=${TPS:-?} | audit rows: $ROWS | $ERR"
  done
done
# Leak test: set_config transactions and actor-less statements share the same pooled server connections.
START=$(maxid)
OUT=$(pgbench -n -M extended -c "$C" -j 4 -T "$T" -f "$D/bench_setcfg.sql@1" -f "$D/bench_plain.sql@1" "$APP_URL" 2>&1); RC=$?
ROWS=$(psql "$ADMIN_URL" -Atc "SELECT string_agg(table_name || ':' || coalesce(actor_source, 'none') || '=' || n, ' ') FROM (SELECT table_name, actor_source, count(*) n FROM auditstream.changes WHERE id > $START GROUP BY 1, 2 ORDER BY 1, 2) x")
echo "$L | leak test | exit=$RC | $ROWS | pass = up_plain all none, up_t all set_config"
