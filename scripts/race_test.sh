#!/usr/bin/env bash
# Concurrency tests for pg_anyfk.
#
# These cannot live in #[pg_test]: proving the parent row lock works needs two
# sessions running at the same time, and those tests get one.
#
# Without `FOR KEY SHARE` in anyfk.check_child(), a concurrent INSERT and DELETE
# both commit and leave a dangling reference. These tests fail loudly if that
# regresses.
#
#   ./scripts/race_test.sh [pg_major]      # default 18
set -euo pipefail

PG="${1:-18}"
DB="anyfk_race"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PG_CONFIG="$(grep -E "^pg${PG} = " "${PGRX_HOME:-$HOME/.pgrx}/config.toml" | sed -E 's/.*"(.*)"/\1/')"
[[ -x "$PG_CONFIG" ]] || { echo "no pg_config for pg${PG}; run cargo pgrx init" >&2; exit 1; }
BIN="$("$PG_CONFIG" --bindir)"
PORT=$((28800 + PG))
PSQL=("$BIN/psql" -h localhost -p "$PORT" -d "$DB" -X -q)

echo "==> installing into pg${PG}"
(cd "$ROOT/ext" && cargo pgrx install --pg-config "$PG_CONFIG" \
    --no-default-features --features "pg${PG}" >/dev/null)
(cd "$ROOT/ext" && cargo pgrx start "pg${PG}" >/dev/null)

"$BIN/psql" -h localhost -p "$PORT" -d postgres -X -q -c "DROP DATABASE IF EXISTS ${DB};"
"$BIN/psql" -h localhost -p "$PORT" -d postgres -X -q -c "CREATE DATABASE ${DB};"

fixture () {
  "${PSQL[@]}" <<'SQL'
DROP TABLE IF EXISTS posts, tags CASCADE;
CREATE EXTENSION IF NOT EXISTS pg_anyfk;
CREATE TABLE tags (id bigint PRIMARY KEY);
INSERT INTO tags SELECT g FROM generate_series(1,100) g;
CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[] NOT NULL DEFAULT '{}');
CREATE INDEX ON posts USING gin (tag_ids);
SELECT anyfk.add('posts(tag_ids)', 'tags(id)');
SQL
}

dangling () {   # how many referenced ids no longer exist
  "${PSQL[@]}" -tAc "
    SELECT count(DISTINCT e) FROM posts, unnest(tag_ids) e
     WHERE NOT EXISTS (SELECT 1 FROM tags WHERE id = e);"
}

fail=0
check () {  # $1 label, $2 expected-blocked(y/n), $3 actual seconds, $4 expect-error(y/n), $5 output
  local ok=1 msg=""
  if [[ "$2" == y && "$3" -lt 2 ]]; then ok=0; msg="expected it to block, waited only $3s"; fi
  if [[ "$4" == y ]] && ! grep -qi 'ERROR' <<<"$5"; then ok=0; msg="expected an error, got: $(head -1 <<<"$5")"; fi
  local d; d=$(dangling)
  if [[ "$d" != 0 ]]; then ok=0; msg="${msg:+$msg; }$d dangling reference(s) left behind"; fi
  if [[ $ok == 1 ]]; then
    echo "PASS $1 (blocked ${3}s, no dangling references)"
  else
    echo "FAIL $1 -- $msg"; fail=1
  fi
}

# ---------------------------------------------------------------------------
echo
echo "==> test 1: INSERT a reference while a DELETE of that parent is attempted"
fixture >/dev/null
# Session A takes FOR KEY SHARE on tag 50 and holds the transaction open.
("${PSQL[@]}" -c "BEGIN; INSERT INTO posts (tag_ids) VALUES ('{50}'); SELECT pg_sleep(6); COMMIT;" >/dev/null 2>&1) &
sleep 2
s=$(date +%s)
out=$("${PSQL[@]}" -c "DELETE FROM tags WHERE id = 50;" 2>&1 || true)
e=$(date +%s)
wait
check "concurrent insert blocks the parent delete" y $((e-s)) y "$out"

# ---------------------------------------------------------------------------
echo
echo "==> test 2: DELETE a parent, then INSERT a reference to it from another session"
fixture >/dev/null
# Session A deletes tag 60 (nothing references it yet) and holds the transaction.
("${PSQL[@]}" -c "BEGIN; DELETE FROM tags WHERE id = 60; SELECT pg_sleep(6); COMMIT;" >/dev/null 2>&1) &
sleep 2
s=$(date +%s)
out=$("${PSQL[@]}" -c "INSERT INTO posts (tag_ids) VALUES ('{60}');" 2>&1 || true)
e=$(date +%s)
wait
check "pending parent delete blocks the child insert" y $((e-s)) y "$out"

# ---------------------------------------------------------------------------
echo
echo "==> test 3: unrelated keys must not block each other"
fixture >/dev/null
("${PSQL[@]}" -c "BEGIN; INSERT INTO posts (tag_ids) VALUES ('{70}'); SELECT pg_sleep(5); COMMIT;" >/dev/null 2>&1) &
sleep 2
s=$(date +%s)
out=$("${PSQL[@]}" -c "DELETE FROM tags WHERE id = 71;" 2>&1 || true)
e=$(date +%s)
wait
if [[ $((e-s)) -lt 2 ]] && ! grep -qi ERROR <<<"$out"; then
  echo "PASS deleting an unreferenced key is not blocked ($((e-s))s)"
else
  echo "FAIL deleting tag 71 should not have blocked: $((e-s))s, $out"; fail=1
fi

# ---------------------------------------------------------------------------
# scrub rewrites the arrays instead of refusing, so the delete succeeds. The
# question is whether a concurrent insert can still slip a reference past it.
fixture_scrub () {
  "${PSQL[@]}" <<'SQL'
DROP TABLE IF EXISTS posts, tags CASCADE;
CREATE EXTENSION IF NOT EXISTS pg_anyfk;
CREATE TABLE tags (id bigint PRIMARY KEY);
INSERT INTO tags SELECT g FROM generate_series(1,100) g;
CREATE TABLE posts (id bigserial PRIMARY KEY, tag_ids bigint[] NOT NULL DEFAULT '{}');
CREATE INDEX ON posts USING gin (tag_ids);
SELECT anyfk.add('posts(tag_ids)', 'tags(id)', on_delete => 'scrub');
SQL
}

echo
echo "==> test 4: scrub -- INSERT a reference while that parent is being deleted"
fixture_scrub >/dev/null
("${PSQL[@]}" -c "BEGIN; INSERT INTO posts (tag_ids) VALUES ('{50}'); SELECT pg_sleep(6); COMMIT;" >/dev/null 2>&1) &
sleep 2
s=$(date +%s)
# Expect: blocks until the insert commits, then scrubs the row it just created.
out=$("${PSQL[@]}" -c "DELETE FROM tags WHERE id = 50;" 2>&1 || true)
e=$(date +%s)
wait
check "scrub waits for the concurrent insert, then cleans it up" y $((e-s)) n "$out"

echo
echo "==> test 5: scrub -- INSERT a reference to a parent already being deleted"
fixture_scrub >/dev/null
("${PSQL[@]}" -c "BEGIN; DELETE FROM tags WHERE id = 60; SELECT pg_sleep(6); COMMIT;" >/dev/null 2>&1) &
sleep 2
s=$(date +%s)
out=$("${PSQL[@]}" -c "INSERT INTO posts (tag_ids) VALUES ('{60}');" 2>&1 || true)
e=$(date +%s)
wait
check "pending scrub delete blocks the child insert" y $((e-s)) y "$out"

echo
if [[ $fail == 0 ]]; then echo "ALL CONCURRENCY TESTS PASSED"; else echo "CONCURRENCY TESTS FAILED"; exit 1; fi
