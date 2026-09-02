#!/usr/bin/env bash
# End-to-end smoke test against a real local Docker daemon.
#
# Exercises the PRD §15 release criteria that do not need a VPS: engine
# creation, two isolated project databases on one engine, cross-account
# isolation, restart persistence, backup/restore, single-resource deletion,
# the MinIO equivalents, and read-only discovery of foreign containers.
#
# Runs entirely inside a throwaway state directory and removes every container
# and volume it created, whether it passes or fails.
#
#   scripts/e2e-local.sh            # build, run, clean up
#   KEEP=1 scripts/e2e-local.sh     # leave the containers for inspection
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export LINF_STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/linf-e2e.XXXXXX")"
BACKUPS="$LINF_STATE_DIR/backups"
BIN="$ROOT/target/debug/linf"

PASS=0
FAIL=0
CURRENT=""
OWNED_DOCKER_RESOURCES=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m   %s\n' "$*"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

# check <description> <expected-exit> <command...>
check() {
  local desc="$1" want="$2"; shift 2
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  if [ "$rc" -eq "$want" ]; then
    ok "$desc"
  else
    bad "$desc (exit $rc, expected $want)"
    printf '%s\n' "$out" | sed 's/^/       /'
  fi
  CURRENT="$out"
}

# contains <description> <needle>  — asserts against the last check's output
contains() {
  if printf '%s' "$CURRENT" | grep -qF -- "$2"; then
    ok "$1"
  else
    bad "$1 (output did not contain '$2')"
    printf '%s\n' "$CURRENT" | sed 's/^/       /'
  fi
}

# Published host port the app actually chose for an engine kind (ENG-007 may
# have substituted it).
engine_port() {
  "$BIN" engine list --json | python3 -c '
import json, sys
kind = sys.argv[1]
for row in json.load(sys.stdin):
    engine = row["engine"]
    if engine["engine"] == kind:
        print(engine["host_port"])
        break
' "$1"
}

cleanup() {
  local rc=$?
  if [ "${KEEP:-0}" = "1" ]; then
    printf '\nKEEP=1: leaving state in %s\n' "$LINF_STATE_DIR"
    return $rc
  fi
  say "cleanup"
  if [ "$OWNED_DOCKER_RESOURCES" -eq 1 ]; then
    docker rm -f linf-postgres-17 linf-minio-latest >/dev/null 2>&1
    docker volume rm linf-pg17-data linf-minio-latest-data >/dev/null 2>&1
  fi
  rm -rf "$LINF_STATE_DIR"
  return $rc
}
trap cleanup EXIT

say "build"
cargo build --quiet || { echo "build failed"; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }

# The engine names are intentionally fixed, so never delete a resource we did
# not create. A previous interrupted E2E run must be cleaned up explicitly.
for container in linf-postgres-17 linf-minio-latest; do
  if docker container inspect "$container" >/dev/null 2>&1; then
    printf 'refusing E2E: container %s already exists\n' "$container" >&2
    exit 2
  fi
done
for volume in linf-pg17-data linf-minio-latest-data; do
  if docker volume inspect "$volume" >/dev/null 2>&1; then
    printf 'refusing E2E: volume %s already exists\n' "$volume" >&2
    exit 2
  fi
done
OWNED_DOCKER_RESOURCES=1

say "doctor and target registration"
check "doctor runs"                       0 "$BIN" doctor
check "local target registers"            0 "$BIN" target add-local --name local
check "target list shows it connected"    0 "$BIN" target list
contains "docker reported reachable" "connected"
check "duplicate target name refused"     2 "$BIN" target add-local --name local

say "postgres engine"
check "engine plan is preview only"       0 "$BIN" engine ensure local postgres 17 --plan
contains "plan marks the container new" "신규"
check "no container created by --plan"    1 docker inspect --type container linf-postgres-17
check "engine ensure creates it"          0 "$BIN" engine ensure local postgres 17
check "container now exists"              0 docker inspect --type container linf-postgres-17
check "engine ensure is idempotent"       0 "$BIN" engine ensure local postgres 17
# The engine may substitute the port when something else already publishes the
# default one (ENG-007), so read back what it actually chose.
PG_PORT="$(engine_port postgres)"
check "port bound to loopback only"       0 docker port linf-postgres-17 5432
contains "loopback binding" "127.0.0.1:${PG_PORT}"
check "managed label present"             0 docker inspect -f '{{index .Config.Labels "local-infra.managed"}}' linf-postgres-17
contains "label is true" "true"

say "two project databases on one engine"
check "first database"                    0 "$BIN" db create --target local --project Letsbid
check "second database"                   0 "$BIN" db create --target local --project Tamche
check "listing shows both"                0 "$BIN" db list
contains "letsbid listed" "letsbid_dev"
contains "tamche listed" "tamche_dev"
check "only one engine container"         0 bash -c 'test "$(docker ps -q --filter name=linf-postgres-17 | wc -l | tr -d " ")" = 1'
check "duplicate database refused"        2 "$BIN" db create --target local --project Letsbid
check "connection test passes"            0 "$BIN" db test letsbid_dev
check "url is printable"                  0 "$BIN" db url letsbid_dev
contains "url points at the engine" "@127.0.0.1:${PG_PORT}/letsbid_dev"
check "env block is printable"            0 "$BIN" db env letsbid_dev
contains "env has DATABASE_URL" "DATABASE_URL=postgresql://"

say "cross-project isolation"
LETSBID_PW="$("$BIN" db env letsbid_dev | sed -n 's/^PGPASSWORD=//p')"
check "letsbid_user reaches its own database" 0 \
  docker exec -e PGPASSWORD="$LETSBID_PW" linf-postgres-17 \
  psql -h "$(docker exec linf-postgres-17 hostname)" -U letsbid_user -d letsbid_dev -c 'select 1'
check "letsbid_user cannot reach tamche_dev" 2 \
  docker exec -e PGPASSWORD="$LETSBID_PW" linf-postgres-17 \
  psql -h "$(docker exec linf-postgres-17 hostname)" -U letsbid_user -d tamche_dev -c 'select 1'

say "durability across restart"
docker exec linf-postgres-17 psql -U linf_admin -d letsbid_dev \
  -c 'create table if not exists smoke(id int primary key)' >/dev/null 2>&1
docker exec linf-postgres-17 psql -U linf_admin -d letsbid_dev \
  -c 'insert into smoke values (1) on conflict do nothing' >/dev/null 2>&1
check "engine restarts"                   0 "$BIN" engine restart local postgres 17
sleep 3
check "row survived the restart"          0 bash -c \
  'docker exec linf-postgres-17 psql -U linf_admin -d letsbid_dev -tAc "select count(*) from smoke" | grep -q "^1$"'

say "backup and restore"
mkdir -p "$BACKUPS"
check "backup runs"                       0 "$BIN" backup run letsbid_dev --out "$BACKUPS"
check "backup is recorded"                0 "$BIN" backup list letsbid_dev
contains "backup marked ok" "ok"
BACKUP_FILE="$(ls -1 "$BACKUPS"/letsbid_dev-*.dump 2>/dev/null | head -1)"
if [ -n "$BACKUP_FILE" ]; then ok "dump file exists"; else bad "dump file exists"; fi
check "restore target database"           0 "$BIN" db create --target local --project Restored
check "restore into it"                   0 "$BIN" backup restore "$BACKUP_FILE" --into restored_dev --yes
check "restored data is present"          0 bash -c \
  'docker exec linf-postgres-17 psql -U linf_admin -d restored_dev -tAc "select count(*) from smoke" | grep -q "^1$"'

say "single-resource deletion"
check "drop needs confirmation"           2 "$BIN" db drop tamche_dev
check "drop with --yes"                   0 "$BIN" db drop tamche_dev --yes
check "tamche is gone"                    0 bash -c \
  '! docker exec linf-postgres-17 psql -U linf_admin -lqt | cut -d"|" -f1 | grep -qw tamche_dev'
check "letsbid survived"                  0 "$BIN" db test letsbid_dev
check "engine still running"              0 bash -c \
  'test "$(docker inspect -f "{{.State.Running}}" linf-postgres-17)" = true'

say "minio engine and buckets"
check "minio engine ensure"               0 "$BIN" engine ensure local minio latest
check "minio container exists"            0 docker inspect --type container linf-minio-latest
S3_PORT="$(engine_port minio)"
check "s3 port on loopback"               0 docker port linf-minio-latest 9000
contains "s3 loopback binding" "127.0.0.1:${S3_PORT}"
check "console port published"            0 docker port linf-minio-latest 9001
check "bucket create"                     0 "$BIN" bucket create --target local --project Letsbid
check "second bucket create"              0 "$BIN" bucket create --target local --project Tamche
check "bucket list"                       0 "$BIN" bucket list
contains "letsbid bucket listed" "letsbid"
check "bucket access test"                0 "$BIN" bucket test letsbid-dev
check "bucket env block"                  0 "$BIN" bucket env letsbid-dev
contains "env has endpoint" "S3_ENDPOINT=http://127.0.0.1:${S3_PORT}"
contains "env has aws aliases" "AWS_ACCESS_KEY_ID="
check "duplicate bucket refused"          2 "$BIN" bucket create --target local --project Letsbid

say "bucket isolation and backup"
check "bucket backup runs"                0 "$BIN" backup run letsbid-dev --out "$BACKUPS"
BUCKET_ARCHIVE="$(ls -1 "$BACKUPS"/letsbid-dev-*.objects 2>/dev/null | head -1)"
if [ -n "$BUCKET_ARCHIVE" ]; then ok "object archive exists"; else bad "object archive exists"; fi
if [ -n "$BUCKET_ARCHIVE" ]; then
  check "bucket restore"                  0 "$BIN" backup restore "$BUCKET_ARCHIVE" --into letsbid-dev --overwrite --yes
fi
check "bucket drop with --yes"            0 "$BIN" bucket drop tamche-dev --yes
check "other bucket survived"             0 "$BIN" bucket test letsbid-dev

say "foreign resources are read-only"
check "discovery lists unmanaged containers" 0 "$BIN" discover local
contains "read-only notice shown" "변경하지 않습니다"
UNMANAGED="$(docker ps --format '{{.Names}}' | grep -v '^linf-' | head -1)"
if [ -n "$UNMANAGED" ]; then
  contains "an unmanaged container is listed" "$UNMANAGED"
fi

say "json output and exit codes"
check "tunnel status as json"             0 "$BIN" tunnel status --json
check "unknown database is exit 2"        2 "$BIN" db url no_such_db
check "unknown subcommand is exit 2"      2 "$BIN" nonsense
check "json error envelope"               2 "$BIN" db url no_such_db --json
contains "json error is machine readable" '"kind": "not_found"'

say "teardown through the app"
check "engine remove needs confirmation"  2 "$BIN" engine rm local postgres 17 --volume
check "engine remove with --yes"          0 "$BIN" engine rm local postgres 17 --volume --yes
check "postgres container gone"           1 docker inspect --type container linf-postgres-17
check "postgres volume gone"              1 docker volume inspect linf-pg17-data
check "minio remove"                      0 "$BIN" engine rm local minio latest --volume --yes
check "minio container gone"              1 docker inspect --type container linf-minio-latest

printf '\n\033[1m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
