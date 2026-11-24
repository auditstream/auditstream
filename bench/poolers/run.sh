#!/usr/bin/env bash
# End-to-end pooler test suite runner:
#   1. Applies fixture roles, tables, seed rows, and grants via ADMIN_URL.
#   2. Generates and installs engine D2 + D3 capture SQL via `pooler_schema`.
#   3. Runs SQL conformance scenarios T1-T9 through APP_URL (pooler) and checks
#      exactly 3 "permission denied" lines + exact sorted capture.sql diff.
#   4. Runs pgbench (16 clients across simple/extended/prepared + leak test).
#
# Required env: ADMIN_URL (direct admin Postgres URL), APP_URL (app_user URL through pooler).
# Optional env: APP_PW (defaults to password parsed from APP_URL, or "app"),
#               POOLER_LABEL (default "pgbouncer"), BENCH_CLIENTS (default 16), BENCH_SECONDS (default 5).
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$DIR/../.." && pwd)

: "${ADMIN_URL:?ADMIN_URL must be set (direct Postgres admin URL)}"
: "${APP_URL:?APP_URL must be set (app_user URL through the pooler)}"

if [ -z "${APP_PW:-}" ]; then
  APP_PW=$(sed -n 's|^[a-zA-Z0-9+.-]*://[^:]*:\([^@]*\)@.*|\1|p' <<<"$APP_URL")
  APP_PW=${APP_PW:-app}
fi

POOLER_LABEL=${POOLER_LABEL:-pgbouncer}
BENCH_CLIENTS=${BENCH_CLIENTS:-16}
BENCH_SECONDS=${BENCH_SECONDS:-5}

echo "==> Step 1: Apply fixtures (roles, tables, seed rows, grants)"
psql "$ADMIN_URL" -v ON_ERROR_STOP=1 -v app_pw="$APP_PW" -f "$DIR/fixtures.sql"

echo "==> Step 2: Generate and install engine capture SQL (D2 + D3)"
(
  cd "$REPO_ROOT"
  cargo run -q -p auditstream-pg --example pooler_schema
) | psql "$ADMIN_URL" -v ON_ERROR_STOP=1

echo "==> Step 3: Run T1-T9 SQL suite through pooler as app_user"
TMPDIR_SUITE=$(mktemp -d)
trap 'rm -rf "$TMPDIR_SUITE"' EXIT

# T9 deliberately attempts 3 forbidden DML statements on auditstream.changes, so do not set ON_ERROR_STOP.
psql "$APP_URL" -f "$DIR/t_app.sql" >"$TMPDIR_SUITE/t_app.out" 2>"$TMPDIR_SUITE/t_app.err" || true

DENIED_COUNT=$(grep -c "permission denied" "$TMPDIR_SUITE/t_app.err" || true)
echo "Tamper denials observed: $DENIED_COUNT (expected 3)"
if [ "$DENIED_COUNT" -ne 3 ]; then
  echo "FAIL: expected exactly 3 'permission denied' lines, got $DENIED_COUNT" >&2
  cat "$TMPDIR_SUITE/t_app.err" >&2
  exit 1
fi

OTHER_ERRORS=$(grep "ERROR:" "$TMPDIR_SUITE/t_app.err" | grep -v "permission denied" || true)
if [ -n "$OTHER_ERRORS" ]; then
  echo "FAIL: unexpected SQL errors during t_app.sql:" >&2
  echo "$OTHER_ERRORS" >&2
  exit 1
fi

psql "$ADMIN_URL" -At -f "$DIR/capture.sql" | LC_ALL=C sort >"$TMPDIR_SUITE/got.txt"
LC_ALL=C sort "$DIR/expected_capture.txt" >"$TMPDIR_SUITE/expected.txt"

if ! diff -u "$TMPDIR_SUITE/expected.txt" "$TMPDIR_SUITE/got.txt"; then
  echo "FAIL: capture.sql output does not match expected_capture.txt" >&2
  exit 1
fi
echo "SQL suite T1-T9 PASSED (17/17 rows matched, 3/3 tamper attempts denied)."

echo "==> Step 4: Run pgbench pooler matrix and leak test ($BENCH_CLIENTS clients, ${BENCH_SECONDS}s)"
"$DIR/bench.sh" "$POOLER_LABEL" "$BENCH_CLIENTS" "$BENCH_SECONDS"
