#!/bin/sh
# Start a built image the way compose.yml runs it (with its Postgres,
# read-only, no capabilities) and wait for /v2/health to answer, which it
# does only once the database is reached and migrated. Everything is
# removed again afterwards, the database volume included.
#
# Usage: tools/smoke-test.sh <image>
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
cd "$(dirname "$0")/.."

MAGNETITE_SYNC_IMAGE=$1
MAGNETITE_SYNC_REGISTRATION=closed
POSTGRES_PASSWORD=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
export MAGNETITE_SYNC_IMAGE MAGNETITE_SYNC_REGISTRATION POSTGRES_PASSWORD

compose() {
  docker compose --project-name magnetite-sync-smoke \
    --file compose.yml --file tools/compose.smoke.yml "$@"
}
trap 'compose down --volumes --timeout 5 >/dev/null 2>&1' EXIT

compose up --detach --no-build --wait
for _ in $(seq 1 60); do
  if [ "$(curl --silent --max-time 2 http://127.0.0.1:8787/v2/health)" = "ok" ]; then
    echo "health: ok"
    exit 0
  fi
  sleep 0.5
done
echo "the server did not answer /v2/health within 30 seconds" >&2
compose logs >&2
exit 1
