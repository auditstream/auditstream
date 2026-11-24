#!/usr/bin/env bash
# usage: bench/poolers/bench.sh <label> [clients] [seconds]
# Needs ADMIN_URL (admin, direct) and APP_URL (app_user through the pooler), with psql and pgbench on PATH.
# Use more clients than the pooler's server-side pool size so clients queue for server connections.
set -euo pipefail

LABEL=${1:-pgbouncer}
CLIENTS=${2:-16}
SECONDS_RUN=${3:-5}
DIR=$(cd "$(dirname "$0")" && pwd)

extract_num() {
  sed -n "s/.*$1[^0-9]*\([0-9][0-9.]*\).*/\1/p" | head -n 1
}

max_id() {
  psql "$ADMIN_URL" -Atc "SELECT coalesce(max(id), 0) FROM auditstream.changes"
}

FAILURES=0

for MODE in simple extended prepared; do
  for SCRIPT in comment setcfg; do
    EXPECTED_SRC="comment"
    if [ "$SCRIPT" = "setcfg" ]; then
      EXPECTED_SRC="set_config"
    fi

    START=$(max_id)
    set +e
    # Use one pgbench thread per client (-j "$CLIENTS"): under -M prepared, pgbench calls
    # synchronous PQprepare() lazily per client, so multiplexing multiple clients onto one
    # thread deadlocks against a smaller transaction-mode server pool during BEGIN;...COMMIT;.
    OUT=$(pgbench -n -M "$MODE" -c "$CLIENTS" -j "$CLIENTS" -T "$SECONDS_RUN" -f "$DIR/bench_${SCRIPT}.sql" "$APP_URL" 2>&1)
    RC=$?
    set -e

    TX=$(extract_num "actually processed" <<<"$OUT")
    FAILED=$(extract_num "failed transactions" <<<"$OUT")
    TPS=$(extract_num "tps =" <<<"$OUT")
    FAILED=${FAILED:-0}

    ROWS=$(psql "$ADMIN_URL" -Atc \
      "SELECT coalesce(string_agg(coalesce(actor_source, 'none') || '=' || n, ' ' ORDER BY 1), 'none')
         FROM (
           SELECT actor_source, count(*) AS n
             FROM auditstream.changes
            WHERE id > $START AND table_name = 'up_t'
            GROUP BY 1
         ) x")

    ERR=$(grep -iE 'error|timeout|abort' <<<"$OUT" | head -n 2 | tr '\n' ' ' || true)
    echo "$LABEL | -M $MODE | $SCRIPT | exit=$RC tx=${TX:-0} failed=$FAILED tps=${TPS:-?} | audit rows: $ROWS | $ERR"

    if [ "$RC" -ne 0 ]; then
      echo "FAIL: pgbench exited with $RC for -M $MODE $SCRIPT" >&2
      FAILURES=$((FAILURES + 1))
      continue
    fi
    if [ "${FAILED}" != "0" ]; then
      echo "FAIL: non-zero failed transactions ($FAILED) for -M $MODE $SCRIPT" >&2
      FAILURES=$((FAILURES + 1))
      continue
    fi
    if [ -z "${TX:-}" ] || [ "$TX" -le 0 ]; then
      echo "FAIL: zero transactions processed for -M $MODE $SCRIPT" >&2
      FAILURES=$((FAILURES + 1))
      continue
    fi
    EXPECTED_SUMMARY="${EXPECTED_SRC}=${TX}"
    if [ "$ROWS" != "$EXPECTED_SUMMARY" ]; then
      echo "FAIL: expected audit rows '$EXPECTED_SUMMARY', got '$ROWS' for -M $MODE $SCRIPT" >&2
      FAILURES=$((FAILURES + 1))
    fi
  done
done

# Leak test: set_config transactions and actor-less statements share the same pooled server connections.
START=$(max_id)
set +e
OUT=$(pgbench -n -M extended -c "$CLIENTS" -j "$CLIENTS" -T "$SECONDS_RUN" -f "$DIR/bench_setcfg.sql@1" -f "$DIR/bench_plain.sql@1" "$APP_URL" 2>&1)
RC=$?
set -e

FAILED=$(extract_num "failed transactions" <<<"$OUT")
FAILED=${FAILED:-0}
ROWS=$(psql "$ADMIN_URL" -Atc \
  "SELECT coalesce(string_agg(table_name || ':' || coalesce(actor_source, 'none') || '=' || n, ' ' ORDER BY table_name, coalesce(actor_source, 'none')), 'none')
     FROM (
       SELECT table_name, actor_source, count(*) AS n
         FROM auditstream.changes
        WHERE id > $START AND table_name IN ('up_plain', 'up_t')
        GROUP BY 1, 2
     ) x")

echo "$LABEL | leak test | exit=$RC failed=$FAILED | $ROWS | pass = up_plain all none, up_t all set_config"

if [ "$RC" -ne 0 ] || [ "$FAILED" != "0" ]; then
  echo "FAIL: leak test pgbench exited with $RC (failed=$FAILED)" >&2
  FAILURES=$((FAILURES + 1))
fi

BAD_LEAK=$(psql "$ADMIN_URL" -Atc \
  "SELECT count(*)
     FROM auditstream.changes
    WHERE id > $START
      AND (
        (table_name = 'up_plain' AND (actor_source IS NOT NULL OR actor IS NOT NULL))
        OR (table_name = 'up_t' AND actor_source IS DISTINCT FROM 'set_config')
      )")
UP_PLAIN_COUNT=$(psql "$ADMIN_URL" -Atc \
  "SELECT count(*) FROM auditstream.changes WHERE id > $START AND table_name = 'up_plain' AND actor_source IS NULL")
UP_T_COUNT=$(psql "$ADMIN_URL" -Atc \
  "SELECT count(*) FROM auditstream.changes WHERE id > $START AND table_name = 'up_t' AND actor_source = 'set_config'")

if [ "$BAD_LEAK" -ne 0 ] || [ "$UP_PLAIN_COUNT" -le 0 ] || [ "$UP_T_COUNT" -le 0 ]; then
  echo "FAIL: leak check failed (bad=$BAD_LEAK up_plain_none=$UP_PLAIN_COUNT up_t_setcfg=$UP_T_COUNT)" >&2
  FAILURES=$((FAILURES + 1))
fi

if [ "$FAILURES" -ne 0 ]; then
  echo "Pooler benchmark suite failed with $FAILURES failure(s)." >&2
  exit 1
fi

echo "Pooler benchmark suite PASSED."
